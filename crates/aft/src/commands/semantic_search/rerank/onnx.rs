//! In-process cross-encoder reranking through ONNX Runtime.
//!
//! The session is raw `ort`, built the way the local embedder builds its own:
//! the runtime library is pre-validated with a plain `dlopen`, bound late if the
//! plugin adopted it after startup, and every native section (session creation,
//! inference, release) runs under the ORT exit gate. The managed-runtime
//! resolver is never called from here: it mutates process-global environment
//! and is only safe at startup.
//!
//! Resource rules:
//! - Its own single-flight worker thread and busy flag, separate from the
//!   embedding worker and its model mutex, so a slow rerank can never make a
//!   query embedding fail as busy.
//! - Intra-op threads come from a budget shared with the embedder (see
//!   [`rerank_intra_threads`]), and they do not busy-wait between operators
//!   (see [`SessionTuning`]).
//! - Requests carry their deadline to the worker; a request whose deadline has
//!   passed when the worker picks it up is dropped without inference, and a
//!   batch stops between sub-batches once the deadline passes.
//! - Model download and session creation never run on the request path. The
//!   first request starts them on the worker thread and is told the backend is
//!   unavailable.
//! - Memory: the model file must fit `MAX_MODEL_FILE_BYTES`, inputs are cut to
//!   `MAX_PAIR_TOKENS`, and one inference never exceeds
//!   `MAX_RERANK_ATTENTION_UNITS` (`batch × tokens²`).

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;

use super::{RerankBackend, RerankDoc, RerankError, RerankFingerprint};

/// One file of a pinned model: its path inside the repository and the sha256
/// of its content at the pinned commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PinnedFile {
    pub(crate) path: &'static str,
    pub(crate) sha256: &'static str,
}

/// A reranker model the ONNX backend may load. Only these can be selected, so a
/// config value can never point the process at an arbitrary download. Each is
/// pinned to one Hugging Face commit and every file to its sha256, so a moved
/// `main` branch can never change the model behind a fingerprint, a recorded
/// fixture pack or a committed order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AllowedModel {
    pub(crate) name: &'static str,
    pub(crate) repo: &'static str,
    /// Hugging Face commit the files are downloaded from; also the
    /// fingerprint revision.
    pub(crate) commit: &'static str,
    pub(crate) model_file: PinnedFile,
    /// Files the ONNX graph loads beside itself (external weights).
    pub(crate) extra_files: &'static [PinnedFile],
    pub(crate) tokenizer_file: PinnedFile,
}

pub(crate) const DEFAULT_MODEL: &str = "bge-reranker-base";

// Commits and hashes were read from the Hugging Face API
// (`/api/models/<repo>/revision/main?blobs=true`, LFS sha256) and, for the
// non-LFS tokenizer files, from downloads at the pinned commit.
pub(crate) const ALLOWED_MODELS: &[AllowedModel] = &[
    AllowedModel {
        name: "bge-reranker-base",
        repo: "BAAI/bge-reranker-base",
        commit: "2cfc18c9415c912f9d8155881c133215df768a70",
        model_file: PinnedFile {
            path: "onnx/model.onnx",
            sha256: "15b9a8c3da82eddf263df571281166e00e9308fe19d077084b642ebfcaf06d2b",
        },
        extra_files: &[],
        tokenizer_file: PinnedFile {
            path: "tokenizer.json",
            sha256: "9eb652ac4e40cc093272bbbe0f55d521cf67570060227109b5cdc20945a4489e",
        },
    },
    AllowedModel {
        name: "bge-reranker-v2-m3",
        repo: "rozgo/bge-reranker-v2-m3",
        commit: "fbd57b17b4db111a9d16813bb08b4c804fac18e9",
        model_file: PinnedFile {
            path: "model.onnx",
            sha256: "3af844cd2de818a95d2b5de5893a336836312c8ade03f53b286ac6beae080321",
        },
        extra_files: &[PinnedFile {
            path: "model.onnx.data",
            sha256: "84b66c787b9b98977a16d5c993a3959210a214c98fc3466da263c949c2068945",
        }],
        tokenizer_file: PinnedFile {
            path: "tokenizer.json",
            sha256: "8bf8afbfd11306bd872018c53bfdf2e160a56f8edbcf49933324404791c148d3",
        },
    },
    AllowedModel {
        name: "jina-reranker-v1-turbo",
        repo: "jinaai/jina-reranker-v1-turbo-en",
        commit: "b8c14f4e723d9e0aab4732a7b7b93741eeeb77c2",
        model_file: PinnedFile {
            path: "onnx/model.onnx",
            sha256: "c1296c66c119de645fa9cdee536d8637740efe85224cfa270281e50f213aa565",
        },
        extra_files: &[],
        tokenizer_file: PinnedFile {
            path: "tokenizer.json",
            sha256: "0046da43cc8c424b317f56b092b0512aaaa65c4f925d2f16af9d9eeb4d0ef902",
        },
    },
    AllowedModel {
        name: "gte-reranker-modernbert-base",
        repo: "Alibaba-NLP/gte-reranker-modernbert-base",
        commit: "f7481e6055501a30fb19d090657df9ec1f79ab2c",
        model_file: PinnedFile {
            path: "onnx/model.onnx",
            sha256: "c6d3226502addbcd4d2cf273802957ebf8a2a6bf94037dcb9b1d95bfc01e5d93",
        },
        extra_files: &[],
        tokenizer_file: PinnedFile {
            path: "tokenizer.json",
            sha256: "2aea6ff4701d063e7e029b6be695a1659f2caaa2ae4fb0e8b18285818271becd",
        },
    },
];

/// Most tokens per pair (query plus candidate). Every allowed model accepts a
/// 512-token input; longer pairs are cut from the longer side
/// first.
pub(crate) const MAX_PAIR_TOKENS: usize = 512;
/// Largest `batch × tokens²` one inference may use. The working memory of one
/// inference (attention tensors and the runtime's arena, which keeps its
/// high-water mark) scales with it. Measured with gte-reranker-modernbert-base
/// on 20 engine-built candidates of about 300 tokens: 2M units held about
/// 1.1 GB above the loaded model, 1M about 0.6 GB, and 500k about 0.27 GB, at
/// the same CPU time per call. 500k is about 2 full-length pairs or 5 pairs of
/// 316 tokens per inference, and the extra sub-batches also let a call stop
/// sooner once its deadline has passed.
pub(crate) const MAX_RERANK_ATTENTION_UNITS: usize = 500_000;
/// Largest model (graph plus external weights) the backend will load. The
/// largest allowed model, bge-reranker-v2-m3 in fp32, is about 2.3 GB.
pub(crate) const MAX_MODEL_FILE_BYTES: u64 = 2_500 * 1024 * 1024;
/// Most pairs accepted in one call.
pub(crate) const MAX_PAIRS_PER_CALL: usize = 64;
/// After a failed download or load, wait this long before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(300);
/// Longest a backend build waits for a model download and load.
const MAX_LOAD_WAIT: Duration = Duration::from_secs(3_600);

/// Intra-op threads for the reranker session, from a budget shared with the
/// embedder. The embedder keeps the count it derives for itself (half the
/// cores, the container CPU quota, at most eight). The reranker takes at most
/// half of that again, and never more than the cores the embedder leaves
/// unused, with a floor of one thread. The two sessions together therefore
/// stay within the available cores whenever there are at least two; on a
/// single core the floor makes it two threads.
pub(crate) fn rerank_intra_threads() -> usize {
    let derivation = crate::local_embed::intra_thread_derivation();
    joint_rerank_threads(
        derivation.threads,
        derivation.available_parallelism,
        derivation.quota_threads,
    )
}

pub(crate) fn joint_rerank_threads(
    embedder_threads: usize,
    available_parallelism: usize,
    quota_threads: Option<usize>,
) -> usize {
    let cores = quota_threads
        .unwrap_or(available_parallelism)
        .min(available_parallelism)
        .max(1);
    embedder_threads
        .div_ceil(2)
        .min(cores.saturating_sub(embedder_threads))
        .max(1)
}

pub(crate) fn find_allowed_model(name: &str) -> Option<&'static AllowedModel> {
    let name = name.trim();
    ALLOWED_MODELS
        .iter()
        .find(|model| model.name.eq_ignore_ascii_case(name))
}

/// Scores query/document pairs. The worker owns one; tests substitute a fake.
pub(crate) trait PairScorer: Send {
    fn score_pairs(
        &mut self,
        query: &str,
        docs: &[String],
        deadline: Instant,
    ) -> Result<Vec<f32>, RerankError>;
}

/// Builds the scorer on the worker thread (download, then session creation).
pub(crate) type ScorerLoader = Arc<dyn Fn() -> Result<Box<dyn PairScorer>, String> + Send + Sync>;

struct Job {
    query: String,
    docs: Vec<String>,
    deadline: Instant,
    reply: Sender<Result<Vec<f32>, RerankError>>,
}

enum WorkerState {
    NotStarted,
    Starting,
    Ready(Sender<Job>),
    Failed { reason: String, at: Instant },
}

struct Shared {
    state: Mutex<WorkerState>,
    /// Set while this reranker has a job queued or running. Another request to
    /// the same reranker is refused as busy instead of queuing behind it.
    busy: AtomicBool,
}

/// The ONNX backend for one allowed model.
pub(crate) struct OnnxReranker {
    fingerprint: RerankFingerprint,
    shared: Arc<Shared>,
    loader: ScorerLoader,
    keep_alive: std::sync::Weak<OnnxReranker>,
}

impl OnnxReranker {
    pub(crate) fn with_loader(fingerprint: RerankFingerprint, loader: ScorerLoader) -> Self {
        Self {
            fingerprint,
            shared: Arc::new(Shared {
                state: Mutex::new(WorkerState::NotStarted),
                busy: AtomicBool::new(false),
            }),
            loader,
            keep_alive: std::sync::Weak::new(),
        }
    }

    fn for_model(
        model: &'static AllowedModel,
        cache_dir: Option<PathBuf>,
        lease: Option<std::sync::Weak<crate::root_cache::CacheLease>>,
    ) -> Self {
        Self::with_loader(
            fingerprint_for(model),
            Arc::new(move || {
                let _lease = match lease.as_ref() {
                    Some(lease) => Some(lease.upgrade().ok_or_else(|| {
                        "cache_dir_unavailable: cache context retired".to_string()
                    })?),
                    None => None,
                };
                let files = match cache_dir.as_deref() {
                    Some(path) => provision_model_files_in(model, path)?,
                    None => provision_model_files(model)?,
                };
                let scorer = OnnxPairScorer::load(model.name, &files, rerank_intra_threads())?;
                Ok(Box::new(scorer) as Box<dyn PairScorer>)
            }),
        )
    }

    /// Start the worker if needed and block until its model is loaded or has
    /// failed to load. Only the backend slot's build thread calls this.
    pub(crate) fn ensure_loaded(&self) -> Result<(), String> {
        let started = Instant::now();
        loop {
            match self.ready_sender() {
                Ok(_) => return Ok(()),
                Err(RerankError::Unavailable(reason)) if reason == "model loading" => {}
                Err(RerankError::Unavailable(reason) | RerankError::Refused(reason))
                | Err(RerankError::Failed(reason)) => return Err(reason),
                Err(RerankError::Timeout) => return Err("timeout".to_string()),
            }
            if started.elapsed() > MAX_LOAD_WAIT {
                return Err("model still loading".to_string());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The worker's queue when it is ready; otherwise start it (once, or again
    /// after `RETRY_AFTER` following a failure) and say why it cannot serve.
    fn ready_sender(&self) -> Result<Sender<Job>, RerankError> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*state {
            WorkerState::Ready(sender) => return Ok(sender.clone()),
            WorkerState::Starting => {
                return Err(RerankError::Unavailable("model loading".to_string()))
            }
            WorkerState::Failed { reason, at } if at.elapsed() < RETRY_AFTER => {
                return Err(RerankError::Unavailable(reason.clone()));
            }
            WorkerState::NotStarted | WorkerState::Failed { .. } => {}
        }
        *state = WorkerState::Starting;
        drop(state);
        let shared = self.shared.clone();
        let loader = self.loader.clone();
        let keep_alive = self.keep_alive.upgrade();
        let spawned = std::thread::Builder::new()
            .name("aft-rerank-onnx".to_string())
            .spawn(move || worker_main(shared, loader, keep_alive));
        if let Err(error) = spawned {
            let reason = format!("worker not started: {error}");
            *self
                .shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = WorkerState::Failed {
                reason: reason.clone(),
                at: Instant::now(),
            };
            return Err(RerankError::Unavailable(reason));
        }
        Err(RerankError::Unavailable("model loading".to_string()))
    }
}

fn worker_main(shared: Arc<Shared>, loader: ScorerLoader, keep_alive: Option<Arc<OnnxReranker>>) {
    let mut scorer = match loader() {
        Ok(scorer) => scorer,
        Err(reason) => {
            crate::slog_warn!("rerank model unavailable: {reason}");
            *shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = WorkerState::Failed {
                reason: format!("model unavailable: {reason}"),
                at: Instant::now(),
            };
            return;
        }
    };
    let (sender, receiver): (Sender<Job>, Receiver<Job>) = crossbeam_channel::unbounded();
    *shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = WorkerState::Ready(sender);
    let weak = Arc::downgrade(&shared);
    drop(keep_alive);
    drop(loader);
    drop(shared);
    for job in receiver {
        let Some(shared) = weak.upgrade() else {
            break;
        };
        // A request whose caller has already given up is dropped, not computed.
        let result = if Instant::now() >= job.deadline {
            Err(RerankError::Timeout)
        } else {
            scorer.score_pairs(&job.query, &job.docs, job.deadline)
        };
        shared.busy.store(false, Ordering::Release);
        let _ = job.reply.send(result);
    }
}

impl OnnxReranker {
    pub(super) fn enqueue(
        &self,
        query: &str,
        docs: &[RerankDoc<'_>],
        deadline: Instant,
    ) -> Result<Receiver<Result<Vec<f32>, RerankError>>, RerankError> {
        if docs.len() > MAX_PAIRS_PER_CALL {
            return Err(RerankError::Refused(format!(
                "{} pairs exceed the limit of {MAX_PAIRS_PER_CALL}",
                docs.len()
            )));
        }
        let sender = self.ready_sender()?;
        if self
            .shared
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(RerankError::Unavailable("busy".to_string()));
        }
        let (reply, response) = crossbeam_channel::bounded(1);
        let job = Job {
            query: query.to_string(),
            docs: docs.iter().map(|doc| doc.text.to_string()).collect(),
            deadline,
            reply,
        };
        if sender.send(job).is_err() {
            self.shared.busy.store(false, Ordering::Release);
            return Err(RerankError::Unavailable("worker stopped".to_string()));
        }
        Ok(response)
    }

    pub(super) fn wait_for_reply(
        response: &Receiver<Result<Vec<f32>, RerankError>>,
        deadline: Instant,
        now: Instant,
    ) -> Result<Vec<f32>, RerankError> {
        match response.recv_timeout(deadline.saturating_duration_since(now)) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(RerankError::Timeout),
            Err(RecvTimeoutError::Disconnected) => {
                Err(RerankError::Failed("worker stopped".to_string()))
            }
        }
    }
}

impl RerankBackend for OnnxReranker {
    fn fingerprint(&self) -> RerankFingerprint {
        self.fingerprint.clone()
    }

    fn max_batch(&self) -> usize {
        MAX_PAIRS_PER_CALL
    }

    fn score(
        &self,
        query: &str,
        docs: &[RerankDoc<'_>],
        deadline: Instant,
    ) -> Result<Vec<f32>, RerankError> {
        let response = self.enqueue(query, docs, deadline)?;
        Self::wait_for_reply(&response, deadline, Instant::now())
    }
}

pub(crate) fn fingerprint_for(model: &AllowedModel) -> RerankFingerprint {
    RerankFingerprint {
        backend: "onnx",
        model: model.name.to_string(),
        revision: model.commit.to_string(),
    }
}

#[cfg(test)]
pub(crate) fn shared_onnx_reranker(model: Option<&str>) -> Result<Arc<OnnxReranker>, String> {
    shared_onnx_reranker_in(model, None, None)
}

/// Keep the legacy process-wide model when no cache location is configured.
/// Configured namespaces share only with the same path and directory identity;
/// weak entries let their workers retire after the last context stops using them.
pub(crate) fn shared_onnx_reranker_in(
    model: Option<&str>,
    cache_dir: Option<PathBuf>,
    lease: Option<std::sync::Weak<crate::root_cache::CacheLease>>,
) -> Result<Arc<OnnxReranker>, String> {
    static DEFAULT_BACKENDS: OnceLock<Mutex<HashMap<&'static str, Arc<OnnxReranker>>>> =
        OnceLock::new();
    type ScopedKey = (&'static str, PathBuf, Option<(u64, u64)>);
    static SCOPED_BACKENDS: OnceLock<Mutex<HashMap<ScopedKey, std::sync::Weak<OnnxReranker>>>> =
        OnceLock::new();
    let requested = model.unwrap_or(DEFAULT_MODEL);
    let allowed = find_allowed_model(requested)
        .ok_or_else(|| format!("unsupported rerank model {requested:?}"))?;
    let make = |cache_dir, lease| {
        Arc::new_cyclic(|weak| {
            let mut backend = OnnxReranker::for_model(allowed, cache_dir, lease);
            backend.keep_alive = weak.clone();
            backend
        })
    };
    let Some(path) = cache_dir else {
        let mut backends = DEFAULT_BACKENDS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        return Ok(backends
            .entry(allowed.name)
            .or_insert_with(|| make(None, None))
            .clone());
    };
    let identity = lease
        .as_ref()
        .and_then(std::sync::Weak::upgrade)
        .map(|lease| lease.root_key());
    let mut backends = SCOPED_BACKENDS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    backends.retain(|_, backend| backend.strong_count() > 0);
    let key = (allowed.name, path.clone(), identity);
    if let Some(backend) = backends.get(&key).and_then(std::sync::Weak::upgrade) {
        return Ok(backend);
    }
    let backend = make(Some(path), lease);
    backends.insert(key, Arc::downgrade(&backend));
    Ok(backend)
}

/// Local paths of one model's files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelFiles {
    pub(crate) model: PathBuf,
    /// External weight files loaded beside the graph.
    pub(crate) extras: Vec<PathBuf>,
    pub(crate) tokenizer: PathBuf,
}

/// Find the model in the shared model cache, downloading it when absent. Runs
/// only on the worker thread.
fn provision_model_files(model: &AllowedModel) -> Result<ModelFiles, String> {
    provision_model_files_in(model, &crate::local_embed::embedding_cache_dir()?)
}

/// Use the pinned snapshot under `cache_dir` when every file is present and
/// matches its pinned sha256; otherwise download the pinned commit and verify
/// it. A file whose hash does not match is deleted and the model refused.
pub(crate) fn provision_model_files_in(
    model: &AllowedModel,
    cache_dir: &Path,
) -> Result<ModelFiles, String> {
    #[cfg(test)]
    if let Some((name, observed)) = PROVISION_PROBE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
    {
        if *name == model.name {
            observed.lock().unwrap().push(cache_dir.to_path_buf());
            return Err("provision path observed".into());
        }
    }
    super::note_io();
    let snapshot = pinned_snapshot_dir(cache_dir, model);
    let local = files_in_snapshot(&snapshot, model);
    if pinned_files(model)
        .iter()
        .all(|file| snapshot.join(file.path).is_file())
    {
        // A cached file that fails its hash is removed here, then downloaded
        // again below.
        if verify_pinned_files(&snapshot, model).is_ok() {
            return Ok(local);
        }
    }

    use hf_hub::api::sync::ApiBuilder;
    // The context's cache lease only proves the cache context is alive, not
    // that `cache_dir` still names its enrolled root. hf-hub creates missing
    // folders with a recursive mkdir, so re-prove the root right before the
    // hand-off; a vanished cache volume refuses instead of downloading onto
    // the main disk.
    crate::root_cache::prepare_third_party_cache_dir(cache_dir)?;
    crate::slog_info!(
        "downloading rerank model {} ({}@{}) to {}",
        model.name,
        model.repo,
        model.commit,
        cache_dir.display()
    );
    let api = ApiBuilder::new()
        .with_progress(false)
        .with_cache_dir(cache_dir.to_path_buf())
        .build()
        .map_err(|error| format!("init model download: {error}"))?;
    let repo = api.repo(hf_hub::Repo::with_revision(
        model.repo.to_string(),
        hf_hub::RepoType::Model,
        model.commit.to_string(),
    ));
    for file in pinned_files(model) {
        repo.get(file.path)
            .map_err(|error| format!("download {}: {error}", file.path))?;
    }
    verify_pinned_files(&snapshot, model)?;
    Ok(local)
}

#[cfg(test)]
type ProvisionProbe = (&'static str, Arc<Mutex<Vec<PathBuf>>>);
#[cfg(test)]
static PROVISION_PROBE: OnceLock<Mutex<Option<ProvisionProbe>>> = OnceLock::new();
#[cfg(test)]
pub(super) fn with_provision_probe<T>(
    model: &'static str,
    work: impl FnOnce() -> T,
) -> (T, Vec<PathBuf>) {
    struct Reset(Option<ProvisionProbe>);
    impl Drop for Reset {
        fn drop(&mut self) {
            *PROVISION_PROBE
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = self.0.take();
        }
    }
    let observed = Arc::new(Mutex::new(Vec::new()));
    let _reset = Reset(
        PROVISION_PROBE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace((model, Arc::clone(&observed))),
    );
    let result = work();
    let paths = observed.lock().unwrap().clone();
    (result, paths)
}

fn pinned_files(model: &AllowedModel) -> Vec<PinnedFile> {
    let mut files = vec![model.model_file];
    files.extend(model.extra_files.iter().copied());
    files.push(model.tokenizer_file);
    files
}

/// hf-hub keeps a repo at `<cache>/models--<org>--<repo>/snapshots/<commit>/`.
pub(crate) fn pinned_snapshot_dir(cache_dir: &Path, model: &AllowedModel) -> PathBuf {
    cache_dir
        .join(format!("models--{}", model.repo.replace('/', "--")))
        .join("snapshots")
        .join(model.commit)
}

fn files_in_snapshot(snapshot: &Path, model: &AllowedModel) -> ModelFiles {
    ModelFiles {
        model: snapshot.join(model.model_file.path),
        extras: model
            .extra_files
            .iter()
            .map(|file| snapshot.join(file.path))
            .collect(),
        tokenizer: snapshot.join(model.tokenizer_file.path),
    }
}

/// Check every file of the snapshot against its pinned sha256. A missing or
/// mismatching file is deleted (the snapshot entry and, when it is a link into
/// hf-hub's blob store, the blob it points to), so no later load can use it.
pub(crate) fn verify_pinned_files(snapshot: &Path, model: &AllowedModel) -> Result<(), String> {
    for file in pinned_files(model) {
        let path = snapshot.join(file.path);
        let actual =
            sha256_file(&path).map_err(|error| format!("hash {}: {error}", path.display()))?;
        if actual != file.sha256 {
            if let Ok(target) = std::fs::canonicalize(&path) {
                let _ = std::fs::remove_file(target);
            }
            let _ = std::fs::remove_file(&path);
            return Err(format!(
                "{} of {}@{} has sha256 {actual}, expected {}; the file was deleted",
                file.path, model.repo, model.commit, file.sha256
            ));
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Total size of the graph file and its external weight files.
fn model_bytes(files: &ModelFiles) -> u64 {
    std::iter::once(&files.model)
        .chain(&files.extras)
        .map(|path| std::fs::metadata(path).map_or(0, |metadata| metadata.len()))
        .sum()
}

/// Session options that decide the reranker's CPU and memory cost. The cost
/// profiling test varies them; production uses `SessionTuning::default()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionTuning {
    /// Whether idle intra-op threads busy-wait for work. ONNX Runtime spins by
    /// default; on a loaded machine the spinning threads compete with the
    /// threads doing the work, and measured on gte-reranker-modernbert-base it
    /// about doubled the CPU time of a call (18 s against 8.5 s for 20
    /// candidates) without making it faster. So it is off.
    pub(crate) intra_op_spinning: bool,
    pub(crate) memory_pattern: bool,
    /// Use ONNX Runtime's CPU memory arena (it keeps freed blocks for reuse).
    pub(crate) cpu_arena: bool,
    /// Ask the arena to release its unused blocks at the end of every run.
    pub(crate) arena_shrinkage: bool,
    /// Largest `batch × tokens²` one inference may use.
    pub(crate) attention_units: usize,
    /// Run the graph through Core ML (Apple GPU / Neural Engine) with these
    /// compute units; `None` is the CPU provider alone. Only the hardware
    /// experiment test sets it.
    pub(crate) coreml: Option<ort::ep::coreml::ComputeUnits>,
    /// Pad every inference to a fixed shape: the sequence length up to the
    /// next of [`LENGTH_BUCKETS`] and the batch up to the most rows that
    /// length allows within `attention_units`, so an accelerator sees a few
    /// static shapes instead of a new one per call. Padding rows repeat the
    /// batch's first row and their scores are dropped.
    pub(crate) length_buckets: bool,
    /// Write an ONNX Runtime profile (per-node timings and providers) with
    /// this file prefix.
    pub(crate) profile_prefix: Option<&'static str>,
    /// Declare the model's `batch_size` and `sequence_length` dimensions as
    /// these fixed values, and pad every inference to exactly that shape
    /// (one session per shape). Only the hardware experiment sets it.
    pub(crate) fixed_shape: Option<(usize, usize)>,
}

/// Sequence lengths inputs are padded to when `length_buckets` is set.
pub(crate) const LENGTH_BUCKETS: [usize; 4] = [128, 256, 384, 512];

fn length_bucket(length: usize) -> usize {
    LENGTH_BUCKETS
        .iter()
        .copied()
        .find(|bucket| *bucket >= length)
        .unwrap_or(length)
}

impl Default for SessionTuning {
    fn default() -> Self {
        Self {
            intra_op_spinning: false,
            memory_pattern: true,
            cpu_arena: true,
            arena_shrinkage: false,
            attention_units: MAX_RERANK_ATTENTION_UNITS,
            coreml: None,
            length_buckets: false,
            profile_prefix: None,
            fixed_shape: None,
        }
    }
}

/// A loaded cross-encoder session and its tokenizer.
pub(crate) struct OnnxPairScorer {
    /// Released explicitly in `Drop` under the ORT exit gate.
    session: ManuallyDrop<Session>,
    tokenizer: Tokenizer,
    wants_token_type_ids: bool,
    pad_id: i64,
    /// Per-run options; set only when ONNX Runtime's CPU memory arena is
    /// shrunk after every run.
    run_options: Option<ort::session::RunOptions>,
    attention_units: usize,
    length_buckets: bool,
    fixed_shape: Option<(usize, usize)>,
}

impl OnnxPairScorer {
    pub(crate) fn load(name: &str, files: &ModelFiles, threads: usize) -> Result<Self, String> {
        Self::load_with(name, files, threads, SessionTuning::default())
    }

    pub(crate) fn load_with(
        name: &str,
        files: &ModelFiles,
        threads: usize,
        tuning: SessionTuning,
    ) -> Result<Self, String> {
        super::note_io();
        let bytes = model_bytes(files);
        if bytes > MAX_MODEL_FILE_BYTES {
            return Err(format!(
                "model {name} is {bytes} bytes, above the {MAX_MODEL_FILE_BYTES}-byte limit"
            ));
        }
        crate::semantic_index::pre_validate_onnx_runtime()?;
        let _ort_section = crate::ort_lifecycle::enter()
            .ok_or_else(|| "rerank model not loaded: the process is shutting down".to_string())?;
        let cpu = ort::ep::CPU::default()
            .with_arena_allocator(tuning.cpu_arena)
            .build();
        let providers = match tuning.coreml {
            Some(units) => vec![
                ort::ep::CoreML::default()
                    .with_model_format(ort::ep::coreml::ModelFormat::MLProgram)
                    .with_compute_units(units)
                    .with_static_input_shapes(tuning.length_buckets)
                    .build()
                    .error_on_failure(),
                cpu,
            ],
            None => vec![cpu],
        };
        let mut builder = Session::builder()
            .map_err(|error| format!("create ONNX session builder: {error}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|error| format!("set ONNX optimization level: {error}"))?
            .with_intra_threads(threads)
            .map_err(|error| format!("set ONNX intra-op threads: {error}"))?
            .with_intra_op_spinning(tuning.intra_op_spinning)
            .map_err(|error| format!("set ONNX intra-op spinning: {error}"))?
            .with_memory_pattern(tuning.memory_pattern)
            .map_err(|error| format!("set ONNX memory pattern: {error}"))?;
        // Fixing both input dimensions lets Core ML compile the whole graph
        // for one shape instead of only the pieces whose shapes it can infer.
        if let Some((batch, length)) = tuning.fixed_shape {
            builder = builder
                .with_dimension_override("batch_size", batch as i64)
                .and_then(|builder| {
                    builder.with_dimension_override("sequence_length", length as i64)
                })
                .map_err(|error| format!("set ONNX dimension overrides: {error}"))?;
        }
        let mut builder = builder
            .with_execution_providers(providers)
            .map_err(|error| format!("set ONNX execution providers: {error}"))?;
        if let Some(prefix) = tuning.profile_prefix {
            builder = builder
                .with_profiling(prefix)
                .map_err(|error| format!("set ONNX profiling: {error}"))?;
        }
        let session = builder
            .commit_from_file(&files.model)
            .map_err(crate::semantic_index::format_embedding_init_error)?;
        let mut tokenizer = Tokenizer::from_file(&files.tokenizer)
            .map_err(|error| format!("load tokenizer {}: {error}", files.tokenizer.display()))?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_PAIR_TOKENS,
                strategy: tokenizers::TruncationStrategy::LongestFirst,
                ..Default::default()
            }))
            .map_err(|error| format!("set tokenizer truncation: {error}"))?;
        tokenizer.with_padding(None);
        let run_options = if tuning.arena_shrinkage {
            let mut options = ort::session::RunOptions::new()
                .map_err(|error| format!("create ONNX run options: {error}"))?;
            options
                .add_config_entry("memory.enable_memory_arena_shrinkage", "cpu:0")
                .map_err(|error| format!("set ONNX arena shrinkage: {error}"))?;
            Some(options)
        } else {
            None
        };
        let wants_token_type_ids = session
            .inputs()
            .iter()
            .any(|input| input.name() == "token_type_ids");
        let pad_id = ["<pad>", "[PAD]"]
            .iter()
            .find_map(|token| tokenizer.token_to_id(token))
            .map_or(0, i64::from);
        crate::slog_info!(
            "rerank model ready: model={name} intra_threads={threads} token_type_ids={wants_token_type_ids}"
        );
        Ok(Self {
            session: ManuallyDrop::new(session),
            tokenizer,
            wants_token_type_ids,
            pad_id,
            run_options,
            attention_units: tuning.attention_units,
            length_buckets: tuning.length_buckets,
            fixed_shape: tuning.fixed_shape,
        })
    }

    fn run_inference(&mut self, encodings: &[tokenizers::Encoding]) -> Result<Vec<f32>, String> {
        let _ort_section = crate::ort_lifecycle::enter()
            .ok_or_else(|| "rerank stopped: the process is shutting down".to_string())?;
        let batch = encodings.len();
        let longest = encodings
            .iter()
            .map(|encoding| encoding.get_ids().len())
            .max()
            .unwrap_or(1)
            .max(1);
        // With fixed shapes, pad the length to its bucket and the batch to the
        // most rows that bucket allows; padding rows repeat the first row.
        let (rows, max_len) = if let Some((batch_rows, length)) = self.fixed_shape {
            (batch_rows.max(batch), length.max(longest))
        } else if self.length_buckets {
            let bucket = length_bucket(longest);
            let fixed = (self.attention_units / (bucket * bucket)).clamp(1, MAX_PAIRS_PER_CALL);
            (fixed.max(batch), bucket)
        } else {
            (batch, longest)
        };
        let mut ids = vec![self.pad_id; rows * max_len];
        let mut mask = vec![0i64; rows * max_len];
        let mut types = vec![0i64; rows * max_len];
        for row in 0..rows {
            let encoding = &encodings[if row < batch { row } else { 0 }];
            let base = row * max_len;
            for (column, ((id, attention), kind)) in encoding
                .get_ids()
                .iter()
                .zip(encoding.get_attention_mask())
                .zip(encoding.get_type_ids())
                .enumerate()
            {
                ids[base + column] = i64::from(*id);
                mask[base + column] = i64::from(*attention);
                types[base + column] = i64::from(*kind);
            }
        }
        let shape = (rows, max_len);
        let input_ids = ndarray::Array2::from_shape_vec(shape, ids)
            .map_err(|error| format!("build input_ids: {error}"))?;
        let attention_mask = ndarray::Array2::from_shape_vec(shape, mask)
            .map_err(|error| format!("build attention_mask: {error}"))?;
        let mut inputs = ort::inputs![
            "input_ids" => Tensor::from_array(input_ids).map_err(|error| format!("input_ids: {error}"))?,
            "attention_mask" => Tensor::from_array(attention_mask).map_err(|error| format!("attention_mask: {error}"))?,
        ];
        if self.wants_token_type_ids {
            let token_type_ids = ndarray::Array2::from_shape_vec(shape, types)
                .map_err(|error| format!("build token_type_ids: {error}"))?;
            inputs.push((
                "token_type_ids".into(),
                Tensor::from_array(token_type_ids)
                    .map_err(|error| format!("token_type_ids: {error}"))?
                    .into(),
            ));
        }
        let outputs = match &self.run_options {
            Some(options) => self.session.run_with_options(inputs, options),
            None => self.session.run(inputs),
        }
        .map_err(|error| format!("rerank inference failed: {error}"))?;
        let output = outputs
            .values()
            .next()
            .ok_or_else(|| "rerank model produced no output".to_string())?;
        let (shape, data): (Vec<i64>, Vec<f32>) = match output.try_extract_tensor::<f32>() {
            Ok((shape, data)) => (shape.to_vec(), data.to_vec()),
            Err(_) => {
                let (shape, data) = output
                    .try_extract_tensor::<half::f16>()
                    .map_err(|error| format!("extract rerank logits: {error}"))?;
                (
                    shape.to_vec(),
                    data.iter().map(|value| value.to_f32()).collect(),
                )
            }
        };
        // Logits are [batch, 1] (or [batch]); the first column is the
        // relevance logit. Its sigmoid is monotonic, so the raw logit orders
        // the same way.
        let width = match shape.as_slice() {
            [count] if *count as usize == rows => 1,
            [count, columns] if *count as usize == rows && *columns >= 1 => *columns as usize,
            other => return Err(format!("unexpected rerank output shape {other:?}")),
        };
        Ok((0..batch).map(|row| data[row * width]).collect())
    }
}

// Only the Unix-only tests that measure scoring cost and run the hardware
// experiment use these: token counts of each truncated (query, document)
// pair, and the runtime profile.
#[cfg(all(test, unix))]
impl OnnxPairScorer {
    /// Write the ONNX Runtime profile now and return its path.
    pub(crate) fn end_profiling(&mut self) -> Result<String, String> {
        self.session
            .end_profiling()
            .map_err(|error| format!("end ONNX profiling: {error}"))
    }

    /// Tokens in each (query, doc) pair after truncation, as scoring sees them.
    pub(crate) fn pair_token_lengths(
        &self,
        query: &str,
        docs: &[String],
    ) -> Result<Vec<usize>, String> {
        let pairs = docs
            .iter()
            .map(|doc| (query, doc.as_str()))
            .collect::<Vec<_>>();
        let encodings = self
            .tokenizer
            .encode_batch(pairs, true)
            .map_err(|error| format!("tokenize: {error}"))?;
        Ok(encodings
            .iter()
            .map(|encoding| encoding.get_ids().len())
            .collect())
    }
}

impl PairScorer for OnnxPairScorer {
    fn score_pairs(
        &mut self,
        query: &str,
        docs: &[String],
        deadline: Instant,
    ) -> Result<Vec<f32>, RerankError> {
        if docs.is_empty() {
            return Ok(Vec::new());
        }
        let pairs = docs
            .iter()
            .map(|doc| (query, doc.as_str()))
            .collect::<Vec<_>>();
        let encodings = self
            .tokenizer
            .encode_batch(pairs, true)
            .map_err(|error| RerankError::Failed(format!("tokenize: {error}")))?;
        let mut scores = Vec::with_capacity(encodings.len());
        // With fixed shapes, batches are cut by bucketed length so a padded
        // batch still fits the attention budget.
        let ranges = if let Some((rows, _)) = self.fixed_shape {
            // One fixed shape: cut into runs of exactly `rows` pairs.
            (0..encodings.len())
                .step_by(rows.max(1))
                .map(|start| start..(start + rows.max(1)).min(encodings.len()))
                .collect()
        } else if self.length_buckets {
            let lengths = encodings
                .iter()
                .map(|encoding| length_bucket(encoding.get_ids().len().max(1)))
                .collect::<Vec<_>>();
            attention_batches_for_lengths(&lengths, self.attention_units)
        } else {
            attention_batches(&encodings, self.attention_units)
        };
        for range in ranges {
            if Instant::now() >= deadline {
                return Err(RerankError::Timeout);
            }
            scores.extend(
                self.run_inference(&encodings[range])
                    .map_err(RerankError::Failed)?,
            );
        }
        Ok(scores)
    }
}

impl Drop for OnnxPairScorer {
    fn drop(&mut self) {
        if let Some(_ort_section) = crate::ort_lifecycle::enter() {
            // SAFETY: `session` is never used again; this is its only drop.
            unsafe { ManuallyDrop::drop(&mut self.session) };
        }
    }
}

/// Split encodings, in order, into runs whose `count × longest²` stays within
/// `budget`. A single over-budget pair still gets a run of its own.
pub(crate) fn attention_batches(
    encodings: &[tokenizers::Encoding],
    budget: usize,
) -> Vec<std::ops::Range<usize>> {
    let lengths = encodings
        .iter()
        .map(|encoding| encoding.get_ids().len().max(1))
        .collect::<Vec<_>>();
    attention_batches_for_lengths(&lengths, budget)
}

pub(crate) fn attention_batches_for_lengths(
    lengths: &[usize],
    budget: usize,
) -> Vec<std::ops::Range<usize>> {
    let mut batches = Vec::new();
    let mut start = 0usize;
    let mut longest = 0usize;
    for (index, length) in lengths.iter().enumerate() {
        let count = index - start;
        let candidate = longest.max(*length);
        let cost = (count + 1)
            .saturating_mul(candidate)
            .saturating_mul(candidate);
        if count > 0 && cost > budget {
            batches.push(start..index);
            start = index;
            longest = *length;
        } else {
            longest = candidate;
        }
    }
    if start < lengths.len() {
        batches.push(start..lengths.len());
    }
    batches
}
