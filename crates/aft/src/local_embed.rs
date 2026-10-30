//! Local ONNX embedding backend (all-MiniLM-L6-v2) driven directly through
//! `ort`.
//!
//! Replaces the `fastembed` crate. We own the ORT session so we can cap
//! intra-op threads — `fastembed` hardcoded `with_intra_threads(all cores)`,
//! which pegged every core during indexing. Builds cap to half of the process's
//! available parallelism, the container CPU quota when present, and eight
//! threads overall. The half-core policy measured 1.7x faster with 3.5x less
//! CPU than oversubscribing all cores; the quota cap prevents ORT from creating
//! host-sized pools inside CPU-constrained containers.
//! Queries use one native thread and inline single-text tokenization, avoiding
//! indexing-sized thread-pool wakeups and queues for sporadic searches.
//!
//! The pipeline reproduces fastembed's MiniLM path byte-for-byte (verified:
//! cosine 1.000000 vs fastembed across code + prose), so existing semantic
//! indexes remain valid with no re-embed:
//!   - tokenizer.json, truncation forced to max_length=512 (the Qdrant
//!     tokenizer ships an embedded max_length=128 that fastembed overrides),
//!     add_special_tokens=true
//!   - ONNX inputs input_ids / attention_mask / token_type_ids (i64)
//!     → output last_hidden_state [batch, seq, dim]
//!   - mean pool: sum(mask · tok, over seq) / max(sum(mask), 1)
//!   - L2 normalize: v / (||v|| + 1e-12)

use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use ort::session::builder::GraphOptimizationLevel;
use ort::session::RunOptions;
use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;

use crate::semantic_index::{format_embedding_init_error, pre_validate_onnx_runtime};
use crate::slog_info;

/// HuggingFace repo fastembed used for all-MiniLM-L6-v2; we reuse the same repo
/// and on-disk cache layout so already-downloaded models are picked up offline.
const MINILM_REPO: &str = "Qdrant/all-MiniLM-L6-v2-onnx";
const MINILM_MODEL_FILE: &str = "model.onnx";
const MINILM_TOKENIZER_FILE: &str = "tokenizer.json";
/// fastembed forces truncation to min(512, model_max_length=512). Existing
/// indexes were built at 512, so we MUST match it — the tokenizer.json itself
/// ships max_length=128, which would silently shorten long inputs and break
/// parity with persisted vectors.
const MINILM_MAX_LENGTH: usize = 512;
/// Per-inference memory budget, expressed in attention units (`batch × max_len²`).
///
/// The transient ONNX attention tensor scales with `batch × heads × seq_len²`,
/// so peak RSS is governed by the *largest single inference*, not total chunk
/// count. Arena shrinkage releases temporary buffers after each run, but cannot
/// reduce that transient peak. Measured:
/// `64 × 512² = 16.78M units → ~4.92 GB peak` — too high for 8–16 GB machines.
///
/// 4.0M units caps the worst case at roughly half that (~2–2.5 GB, re-measured):
/// at 512-token chunks it allows ~15 per inference; at ≤250 tokens (the common
/// case for code symbols) it allows the full 64-chunk batch, so short-chunk
/// throughput is unaffected and only long-chunk batches are split.
const MAX_BATCH_ATTENTION_UNITS: usize = 4_000_000;

const MAX_ORT_INTRA_THREADS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CgroupCpuQuota {
    Limited(usize),
    Unlimited,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IntraThreadDerivation {
    pub(crate) threads: usize,
    pub(crate) source: &'static str,
    pub(crate) available_parallelism: usize,
    pub(crate) quota_threads: Option<usize>,
}

fn quota_threads(quota: u64, period: u64) -> Option<usize> {
    if quota == 0 || period == 0 {
        return None;
    }
    usize::try_from(quota.div_ceil(period))
        .ok()
        .map(|value| value.max(1))
}

fn parse_cgroup_v2_cpu_max(contents: &str) -> CgroupCpuQuota {
    let mut fields = contents.split_whitespace();
    let Some(quota) = fields.next() else {
        return CgroupCpuQuota::Invalid;
    };
    let Some(period) = fields.next() else {
        return CgroupCpuQuota::Invalid;
    };
    if fields.next().is_some() {
        return CgroupCpuQuota::Invalid;
    }
    if quota == "max" {
        return period
            .parse::<u64>()
            .ok()
            .filter(|period| *period > 0)
            .map_or(CgroupCpuQuota::Invalid, |_| CgroupCpuQuota::Unlimited);
    }
    match (quota.parse::<u64>().ok(), period.parse::<u64>().ok()) {
        (Some(quota), Some(period)) => quota_threads(quota, period)
            .map(CgroupCpuQuota::Limited)
            .unwrap_or(CgroupCpuQuota::Invalid),
        _ => CgroupCpuQuota::Invalid,
    }
}

fn parse_cgroup_v1_cpu_quota(quota: &str, period: &str) -> CgroupCpuQuota {
    let Ok(quota) = quota.trim().parse::<i64>() else {
        return CgroupCpuQuota::Invalid;
    };
    let Ok(period) = period.trim().parse::<u64>() else {
        return CgroupCpuQuota::Invalid;
    };
    if quota < 0 {
        return if period > 0 {
            CgroupCpuQuota::Unlimited
        } else {
            CgroupCpuQuota::Invalid
        };
    }
    quota_threads(quota as u64, period)
        .map(CgroupCpuQuota::Limited)
        .unwrap_or(CgroupCpuQuota::Invalid)
}

fn derive_intra_threads(
    available_parallelism: usize,
    v2_cpu_max: Option<&str>,
    v1_cpu_quota_us: Option<&str>,
    v1_cpu_period_us: Option<&str>,
) -> IntraThreadDerivation {
    let available_parallelism = available_parallelism.max(1);
    let parallelism_threads = available_parallelism.div_ceil(2).max(1);
    let quota = match v2_cpu_max.map(parse_cgroup_v2_cpu_max) {
        Some(CgroupCpuQuota::Invalid) | None => match (v1_cpu_quota_us, v1_cpu_period_us) {
            (Some(quota), Some(period)) => parse_cgroup_v1_cpu_quota(quota, period),
            _ => CgroupCpuQuota::Invalid,
        },
        Some(quota) => quota,
    };
    let quota_threads = match quota {
        CgroupCpuQuota::Limited(threads) => Some(threads),
        CgroupCpuQuota::Unlimited | CgroupCpuQuota::Invalid => None,
    };
    let threads = parallelism_threads
        .min(quota_threads.unwrap_or(usize::MAX))
        .min(MAX_ORT_INTRA_THREADS)
        .max(1);
    let source = if quota_threads.is_some_and(|quota| quota <= threads) {
        "quota"
    } else if parallelism_threads > MAX_ORT_INTRA_THREADS {
        "cap"
    } else {
        "parallelism"
    };
    IntraThreadDerivation {
        threads,
        source,
        available_parallelism,
        quota_threads,
    }
}

#[cfg(target_os = "linux")]
fn read_first(paths: &[&str]) -> Option<String> {
    paths
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
}

pub(crate) fn intra_thread_derivation() -> IntraThreadDerivation {
    let available_parallelism = std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(1);
    #[cfg(target_os = "linux")]
    {
        let v2 = read_first(&["/sys/fs/cgroup/cpu.max"]);
        let v1_quota = read_first(&[
            "/sys/fs/cgroup/cpu/cpu.cfs_quota_us",
            "/sys/fs/cgroup/cpu.cfs_quota_us",
        ]);
        let v1_period = read_first(&[
            "/sys/fs/cgroup/cpu/cpu.cfs_period_us",
            "/sys/fs/cgroup/cpu.cfs_period_us",
        ]);
        return derive_intra_threads(
            available_parallelism,
            v2.as_deref(),
            v1_quota.as_deref(),
            v1_period.as_deref(),
        );
    }
    #[cfg(not(target_os = "linux"))]
    derive_intra_threads(available_parallelism, None, None, None)
}

/// Roots share a query session and a separate build/refresh session. Only the
/// build session can unload while its handles survive: sporadic queries must
/// not pay a model reload or wait behind a large indexing inference.
pub struct LocalEmbedder {
    shared: Arc<Mutex<ResidentModel>>,
    // Reload through the requesting root's connected cache, never an unrelated
    // default directory after its shared build session has expired.
    cache_dir: Option<PathBuf>,
    // A root's build/refresh handle keeps queries warm even after its idle
    // native build state unloads. Both sessions drop with their last lease.
    _query_keepalive: Option<Arc<Mutex<ResidentModel>>>,
}

struct ResidentModel {
    model: Option<NativeEmbedder>,
    last_used: Instant,
    role: InferenceRole,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InferenceRole {
    Build,
    Query,
}

const MODEL_IDLE_TTL: Duration = Duration::from_secs(60);
// MiniLM and its lowercase alias are the only accepted model ids. Each role
// shares one session across every root, rather than allocating per-root copies.
static BUILD_MODEL: Mutex<Option<Weak<Mutex<ResidentModel>>>> = Mutex::new(None);
static QUERY_MODEL: Mutex<Option<Weak<Mutex<ResidentModel>>>> = Mutex::new(None);
static MODEL_REAPER: OnceLock<Result<(), String>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EmbeddingSessionMemory {
    pub sessions: u64,
    /// Serialized model size, an estimate of weights, not a native heap reading.
    pub model_bytes: u64,
}

static SESSION_MEMORY: Mutex<EmbeddingSessionMemory> = Mutex::new(EmbeddingSessionMemory {
    sessions: 0,
    model_bytes: 0,
});

pub(crate) fn session_memory() -> EmbeddingSessionMemory {
    *SESSION_MEMORY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl ResidentModel {
    fn release_if_idle(&mut self, now: Instant) {
        if self.role == InferenceRole::Build
            && now.saturating_duration_since(self.last_used) >= MODEL_IDLE_TTL
        {
            self.model.take();
        }
    }
}

impl LocalEmbedder {
    pub fn new(model: &str) -> Result<Self, String> {
        Self::with_cache_dir(model, None)
    }

    pub(crate) fn with_cache_dir(model: &str, cache_dir: Option<&Path>) -> Result<Self, String> {
        validate_model(model)?;
        // Initialize the query model before background work becomes available.
        // Its registry and inference lock never depend on the build session.
        let query_keepalive = shared_model(model, InferenceRole::Query, cache_dir)?;
        let shared = shared_model(model, InferenceRole::Build, cache_dir)?;
        MODEL_REAPER
            .get_or_init(|| {
                std::thread::Builder::new()
                    .name("embedding-idle".into())
                    .spawn(|| loop {
                        std::thread::sleep(Duration::from_secs(15));
                        let shared = BUILD_MODEL
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .as_ref()
                            .and_then(Weak::upgrade);
                        if let Some(shared) = shared {
                            // Never queue behind an inference: its completion renews
                            // the idle clock, and its session must remain resident.
                            match shared.try_lock() {
                                Ok(mut resident) => resident.release_if_idle(Instant::now()),
                                Err(std::sync::TryLockError::Poisoned(error)) => {
                                    error.into_inner().release_if_idle(Instant::now());
                                }
                                Err(std::sync::TryLockError::WouldBlock) => {}
                            }
                        }
                    })
                    .map(|_| ())
                    .map_err(|error| format!("failed to start embedding idle reaper: {error}"))
            })
            .as_ref()
            .map_err(Clone::clone)?;
        Ok(Self {
            shared,
            cache_dir: cache_dir.map(Path::to_path_buf),
            _query_keepalive: Some(query_keepalive),
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_query(model: &str) -> Result<Self, String> {
        Self::with_query_cache_dir(model, None)
    }

    pub(crate) fn with_query_cache_dir(
        model: &str,
        cache_dir: Option<&Path>,
    ) -> Result<Self, String> {
        validate_model(model)?;
        Ok(Self {
            shared: shared_model(model, InferenceRole::Query, cache_dir)?,
            cache_dir: cache_dir.map(Path::to_path_buf),
            _query_keepalive: None,
        })
    }

    pub fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // ort::Session::run requires exclusive access. Builds serialize with
        // builds, queries with queries; neither role holds the other's lock.
        let mut resident = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if resident.model.is_none() {
            resident.model = Some(NativeEmbedder::with_cache_dir(
                "all-MiniLM-L6-v2",
                resident.role,
                self.cache_dir.as_deref(),
            )?);
        }
        let result = resident.model.as_mut().unwrap().embed(texts);
        resident.last_used = Instant::now();
        result
    }
}

fn shared_model(
    model: &str,
    role: InferenceRole,
    cache_dir: Option<&Path>,
) -> Result<Arc<Mutex<ResidentModel>>, String> {
    let registry = match role {
        InferenceRole::Build => &BUILD_MODEL,
        InferenceRole::Query => &QUERY_MODEL,
    };
    let mut registry = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(shared) = registry.as_ref().and_then(Weak::upgrade) {
        return Ok(shared);
    }
    let shared = Arc::new(Mutex::new(ResidentModel {
        model: Some(NativeEmbedder::with_cache_dir(model, role, cache_dir)?),
        last_used: Instant::now(),
        role,
    }));
    *registry = Some(Arc::downgrade(&shared));
    Ok(shared)
}

fn validate_model(model: &str) -> Result<(), String> {
    match model {
        "all-MiniLM-L6-v2" | "all-minilm-l6-v2" => Ok(()),
        other => Err(format!(
            "unsupported local embedding model '{other}'. Supported: all-MiniLM-L6-v2"
        )),
    }
}

struct NativeEmbedder {
    /// Released explicitly in `Drop` so the native release can be skipped
    /// once the process has started exiting (see `crate::ort_lifecycle`).
    session: ManuallyDrop<Session>,
    tokenizer: Tokenizer,
    wants_token_type_ids: bool,
    run_options: ManuallyDrop<RunOptions>,
    model_bytes: u64,
    role: InferenceRole,
    #[cfg(test)]
    model_path: PathBuf,
}

impl NativeEmbedder {
    /// Build the embedder for the named model. Only `all-MiniLM-L6-v2` is
    /// supported as the local backend (matches the prior fastembed surface).
    #[cfg(test)]
    fn new(model: &str, role: InferenceRole) -> Result<Self, String> {
        Self::with_cache_dir(model, role, None)
    }

    fn with_cache_dir(
        model: &str,
        role: InferenceRole,
        cache_dir: Option<&Path>,
    ) -> Result<Self, String> {
        validate_model(model)?;

        // Fail with an actionable message instead of letting ort panic deep
        // inside dlopen on an incompatible/absent ONNX Runtime.
        pre_validate_onnx_runtime()?;

        let (model_path, tokenizer_path) = resolve_model_files(cache_dir)?;

        let thread_derivation = intra_thread_derivation();
        // Short queries run on their caller instead of waking an indexing-sized
        // native thread pool after each idle interval. Builds retain their cap.
        let threads = if role == InferenceRole::Query {
            1
        } else {
            thread_derivation.threads
        };
        // Environment creation and model loading are native ORT work; hold the
        // exit gate so process exit cannot destroy ORT's statics underneath it.
        let _ort_section = crate::ort_lifecycle::enter().ok_or_else(|| {
            "semantic embedder not started: the process is shutting down".to_string()
        })?;
        let session = Session::builder()
            .map_err(|e| format!("failed to create ONNX session builder: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| format!("failed to set ONNX optimization level: {e}"))?
            .with_intra_threads(threads)
            .map_err(|e| format!("failed to set ONNX intra-op threads: {e}"))?
            .commit_from_file(&model_path)
            // Route through the shared formatter so a missing/incompatible ONNX
            // Runtime (dlopen failure) yields the actionable install hint rather
            // than a raw ort error.
            .map_err(format_embedding_init_error)?;

        let mut run_options =
            RunOptions::new().map_err(|e| format!("failed to create ONNX run options: {e}"))?;
        // Builds return their large temporary tensors after each run. Queries
        // embed one truncated text and keep their much smaller arena warm, so
        // sporadic searches reuse buffers instead of reallocating them.
        if role == InferenceRole::Build {
            run_options
                .add_config_entry("memory.enable_memory_arena_shrinkage", "cpu:0")
                .map_err(|e| format!("failed to enable ONNX CPU arena shrinkage: {e}"))?;
        }

        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| format!("failed to load tokenizer {}: {e}", tokenizer_path.display()))?;
        // Override the tokenizer's embedded truncation (Qdrant ships 128) to 512
        // for parity with fastembed and existing indexes.
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MINILM_MAX_LENGTH,
                ..Default::default()
            }))
            .map_err(|e| format!("failed to set tokenizer truncation: {e}"))?;

        let wants_token_type_ids = session
            .inputs()
            .iter()
            .any(|input| input.name() == "token_type_ids");

        slog_info!(
            "local embedder ready: model=all-MiniLM-L6-v2 intra_threads={} intra_threads_source={} available_parallelism={} cgroup_quota_threads={} token_type_ids={}",
            threads,
            if role == InferenceRole::Query { "query_latency" } else { thread_derivation.source },
            thread_derivation.available_parallelism,
            thread_derivation
                .quota_threads
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string()),
            wants_token_type_ids
        );

        let model_bytes = std::fs::metadata(&model_path)
            .map_err(|e| format!("failed to measure embedding model: {e}"))?
            .len();
        {
            let mut memory = SESSION_MEMORY
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            memory.sessions += 1;
            memory.model_bytes += model_bytes;
        }
        Ok(Self {
            session: ManuallyDrop::new(session),
            tokenizer,
            wants_token_type_ids,
            run_options: ManuallyDrop::new(run_options),
            model_bytes,
            role,
            #[cfg(test)]
            model_path,
        })
    }

    /// Embed a batch of texts → one L2-normalized 384-dim vector each.
    ///
    /// Internally sub-batches by a token budget so a single ONNX inference can
    /// never balloon peak RSS: the transient attention tensor scales with
    /// `batch × heads × seq_len²`, so a batch that happens to contain many
    /// long (512-token) chunks would otherwise spike memory (~5 GB worst case
    /// at batch=64 × 512 tokens). We cap `batch × max_len²` per inference,
    /// which keeps short-chunk batches at full size (no throughput loss) while
    /// splitting long-chunk batches into smaller inferences. Output order and
    /// vectors are identical to embedding the whole input in one call.
    pub fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // A single query must not queue on the process-wide Rayon pool that
        // background chunk collection and batch tokenization also use.
        let encodings = if self.role == InferenceRole::Query && texts.len() == 1 {
            vec![self
                .tokenizer
                .encode(texts[0].as_str(), true)
                .map_err(|e| format!("tokenize query: {e}"))?]
        } else {
            let text_refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            self.tokenizer
                .encode_batch(text_refs, true)
                .map_err(|e| format!("tokenize batch: {e}"))?
        };

        // Greedily partition (order-preserving) into sub-batches bounded by the
        // attention-unit budget. `cost = (count) × max_len²`; flush before
        // adding a row that would exceed the budget.
        let mut result = Vec::with_capacity(encodings.len());
        let mut batch_start = 0usize;
        let mut batch_max = 0usize;
        for (i, enc) in encodings.iter().enumerate() {
            let len = enc.get_ids().len().max(1);
            let count = i - batch_start; // size BEFORE adding row i
            let candidate_max = batch_max.max(len);
            let cost = (count + 1)
                .saturating_mul(candidate_max)
                .saturating_mul(candidate_max);
            if count > 0 && cost > MAX_BATCH_ATTENTION_UNITS {
                let vecs = self.run_inference(&encodings[batch_start..i])?;
                result.extend(vecs);
                batch_start = i;
                batch_max = len;
            } else {
                batch_max = candidate_max;
            }
        }
        // Flush the final sub-batch (encodings is non-empty here).
        let vecs = self.run_inference(&encodings[batch_start..])?;
        result.extend(vecs);
        Ok(result)
    }

    /// Run one ONNX inference over a single sub-batch of pre-tokenized
    /// encodings: pad to the sub-batch longest, run the model, mean-pool over
    /// the attention mask, L2-normalize. Memory here is bounded by the caller
    /// (`embed`) via the attention-unit budget.
    fn run_inference(
        &mut self,
        encodings: &[tokenizers::Encoding],
    ) -> Result<Vec<Vec<f32>>, String> {
        if encodings.is_empty() {
            return Ok(Vec::new());
        }
        // Tensor creation, inference and output extraction all call into ORT.
        let _ort_section = crate::ort_lifecycle::enter().ok_or_else(|| {
            "semantic embedding stopped: the process is shutting down".to_string()
        })?;

        let batch = encodings.len();
        let max_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(1)
            .max(1);

        // Pad to the batch-longest. The attention mask zeroes padding inside the
        // model's attention and the mean-pool below ignores it, so a padded
        // batch yields identical vectors to embedding each text alone.
        let mut ids = vec![0i64; batch * max_len];
        let mut mask = vec![0i64; batch * max_len];
        for (row, enc) in encodings.iter().enumerate() {
            let row_ids = enc.get_ids();
            let row_mask = enc.get_attention_mask();
            let base = row * max_len;
            for col in 0..row_ids.len() {
                ids[base + col] = row_ids[col] as i64;
                mask[base + col] = row_mask[col] as i64;
            }
        }

        let input_ids = ndarray::Array2::<i64>::from_shape_vec((batch, max_len), ids)
            .map_err(|e| format!("build input_ids tensor: {e}"))?;
        let attention_mask = ndarray::Array2::<i64>::from_shape_vec((batch, max_len), mask)
            .map_err(|e| format!("build attention_mask tensor: {e}"))?;

        let mut inputs = ort::inputs![
            "input_ids" => Tensor::from_array(input_ids).map_err(|e| format!("input_ids: {e}"))?,
            "attention_mask" => Tensor::from_array(attention_mask.clone())
                .map_err(|e| format!("attention_mask: {e}"))?,
        ];
        if self.wants_token_type_ids {
            let token_type_ids = ndarray::Array2::<i64>::zeros((batch, max_len));
            inputs.push((
                "token_type_ids".into(),
                Tensor::from_array(token_type_ids)
                    .map_err(|e| format!("token_type_ids: {e}"))?
                    .into(),
            ));
        }

        let outputs = self
            .session
            .run_with_options(inputs, &self.run_options)
            .map_err(|e| format!("ONNX inference failed: {e}"))?;
        let output = outputs
            .values()
            .next()
            .ok_or_else(|| "ONNX model produced no output".to_string())?;

        // last_hidden_state may be f32 (standard) or f16 (uniform-fp16 exports).
        let (shape, data): (Vec<i64>, Vec<f32>) = match output.try_extract_tensor::<f32>() {
            Ok((s, d)) => (s.to_vec(), d.to_vec()),
            Err(_) => {
                let (s, d) = output
                    .try_extract_tensor::<half::f16>()
                    .map_err(|e| format!("extract output tensor: {e}"))?;
                (s.to_vec(), d.iter().map(|h| h.to_f32()).collect())
            }
        };
        if shape.len() != 3 {
            return Err(format!(
                "unexpected ONNX output rank {} (expected 3: [batch, seq, dim])",
                shape.len()
            ));
        }
        let seq = shape[1] as usize;
        let dim = shape[2] as usize;

        let mut result = Vec::with_capacity(batch);
        for row in 0..batch {
            let mut emb = vec![0.0f32; dim];
            let mut valid = 0.0f32;
            for col in 0..seq {
                if mask_at(&attention_mask, row, col) == 1 {
                    valid += 1.0;
                    let base = (row * seq + col) * dim;
                    for (d, slot) in emb.iter_mut().enumerate() {
                        *slot += data[base + d];
                    }
                }
            }
            let denom = if valid == 0.0 { 1.0 } else { valid };
            for slot in &mut emb {
                *slot /= denom;
            }
            let norm = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
            for slot in &mut emb {
                *slot /= norm + 1e-12;
            }
            result.push(emb);
        }
        Ok(result)
    }
}

impl Drop for NativeEmbedder {
    fn drop(&mut self) {
        match crate::ort_lifecycle::enter() {
            Some(_ort_section) => {
                // SAFETY: native values are never used again; these are their
                // only drops, protected against ORT static teardown.
                unsafe {
                    ManuallyDrop::drop(&mut self.run_options);
                    ManuallyDrop::drop(&mut self.session);
                }
            }
            // The process is exiting. Releasing the session now could race
            // ORT's own static teardown, and the OS reclaims the memory anyway.
            None => {}
        }
        let mut memory = SESSION_MEMORY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        memory.sessions -= 1;
        memory.model_bytes -= self.model_bytes;
    }
}

#[inline]
fn mask_at(mask: &ndarray::Array2<i64>, row: usize, col: usize) -> i64 {
    mask[[row, col]]
}

/// Resolve the MiniLM model.onnx + tokenizer.json, reusing an existing local
/// download when present (offline-safe) and falling back to an hf-hub fetch.
fn resolve_model_files(cache_dir: Option<&Path>) -> Result<(PathBuf, PathBuf), String> {
    let cache_dir = match cache_dir {
        Some(path) => path.to_path_buf(),
        None => embedding_cache_dir()?,
    };

    if let Some(found) = scan_local_snapshot(&cache_dir) {
        return Ok(found);
    }

    // Not cached locally — download via hf-hub into the same cache layout so a
    // subsequent run finds it through the local scan above.
    download_via_hf_hub(&cache_dir)
}

/// fastembed read `FASTEMBED_CACHE_DIR`; the bridge/warmup set it to
/// `<storage>/semantic/models`. Keep the same env + default so existing
/// downloads are reused; an absolute `XDG_CACHE_HOME` outranks the `~/.cache`
/// default.
pub(crate) fn embedding_cache_dir() -> Result<PathBuf, String> {
    embedding_cache_dir_from(
        |name| crate::environment::non_empty_os_var(name),
        std::env::home_dir().as_deref(),
    )
    .ok_or_else(|| "could not determine a home directory for the fastembed cache".to_string())
}

fn embedding_cache_dir_from(
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
    fallback_home: Option<&Path>,
) -> Option<PathBuf> {
    let non_empty = |name| lookup(name).filter(|value| !value.is_empty());
    if let Some(dir) = non_empty("FASTEMBED_CACHE_DIR") {
        return Some(PathBuf::from(dir));
    }
    // An absolute XDG_CACHE_HOME is the user's declared cache home, so a
    // redirected environment (a test gate, a sandbox) never reaches into the
    // real `~/.cache`. A relative value is ignored, per the XDG spec.
    if let Some(dir) = non_empty("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return Some(dir.join("fastembed"));
    }
    non_empty("HOME")
        .or_else(|| non_empty("USERPROFILE"))
        .map(PathBuf::from)
        .or_else(|| fallback_home.map(PathBuf::from))
        .map(|home| home.join(".cache").join("fastembed"))
}

/// hf-hub stores repos at `<cache>/models--<org>--<repo>/snapshots/<rev>/`.
/// Find the newest snapshot that has both required files.
fn scan_local_snapshot(cache_dir: &std::path::Path) -> Option<(PathBuf, PathBuf)> {
    let repo_dir = cache_dir.join("models--Qdrant--all-MiniLM-L6-v2-onnx");
    let snapshots = repo_dir.join("snapshots");
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&snapshots)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    // Newest snapshot first (by modified time) so a refreshed revision wins.
    candidates.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    });
    candidates.reverse();
    for snap in candidates {
        let model = snap.join(MINILM_MODEL_FILE);
        let tokenizer = snap.join(MINILM_TOKENIZER_FILE);
        if model.is_file() && tokenizer.is_file() {
            return Some((model, tokenizer));
        }
    }
    None
}

fn download_via_hf_hub(cache_dir: &std::path::Path) -> Result<(PathBuf, PathBuf), String> {
    use hf_hub::api::sync::ApiBuilder;

    // hf-hub creates missing folders with a recursive mkdir. Re-prove the
    // cache root right before the hand-off so a vanished cache volume refuses
    // the download instead of letting it land on the main disk.
    crate::root_cache::prepare_third_party_cache_dir(cache_dir)?;
    slog_info!(
        "downloading all-MiniLM-L6-v2 ({}) to {}",
        MINILM_REPO,
        cache_dir.display()
    );
    let api = ApiBuilder::new()
        .with_progress(false)
        .with_cache_dir(cache_dir.to_path_buf())
        .build()
        .map_err(|e| format!("failed to init hf-hub api: {e}"))?;
    let repo = api.model(MINILM_REPO.to_string());
    let model = repo
        .get(MINILM_MODEL_FILE)
        .map_err(|e| format!("failed to download {MINILM_MODEL_FILE}: {e}"))?;
    let tokenizer = repo
        .get(MINILM_TOKENIZER_FILE)
        .map_err(|e| format!("failed to download {MINILM_TOKENIZER_FILE}: {e}"))?;
    Ok((model, tokenizer))
}

#[cfg(test)]
mod tests {
    use super::{
        derive_intra_threads, embedding_cache_dir_from, parse_cgroup_v1_cpu_quota,
        parse_cgroup_v2_cpu_max, CgroupCpuQuota, MINILM_MAX_LENGTH,
    };
    use std::io::Write;
    use tokenizers::Tokenizer;

    /// A cache volume that disappears after configure and leaves a writable
    /// empty folder at its path must not receive the model download: hf-hub
    /// would recreate the missing folders and put the model on the main disk.
    #[test]
    fn cache_root_model_download_refuses_a_replaced_cache_root() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let volume = temp.path().join("cache-volume");
        std::fs::create_dir(&volume).unwrap();
        let mut config = crate::config::Config {
            storage_dir: Some(storage.clone()),
            cache_dir: Some(volume.clone()),
            ..Default::default()
        };
        let warning = crate::root_cache::prepare_cache_root_for_test(&mut config);
        assert!(warning.is_none(), "cache root not enrolled: {warning:?}");
        let models = config.semantic.local_cache_dir.clone().unwrap();
        // Windows cannot remove a folder its open cache lease still holds.
        drop(config);
        std::fs::remove_dir_all(&volume).unwrap();
        std::fs::create_dir(&volume).unwrap();

        let error = super::resolve_model_files(Some(&models)).unwrap_err();

        assert!(
            error.starts_with(crate::root_cache::CACHE_DIR_UNAVAILABLE),
            "{error}"
        );
        assert_eq!(
            std::fs::read_dir(&volume).unwrap().count(),
            0,
            "the download recreated folders on the replacement volume"
        );
        assert!(!storage.join("semantic").exists());
    }

    /// The same refusal on a real second filesystem: the cache folder on the
    /// mounted volume is replaced by an empty folder at the same path.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires AFT_TEST_CACHE_VOLUME on a real mounted second filesystem"]
    fn macos_real_cache_volume_model_download_refuses_a_replaced_cache_root() {
        let volume = std::fs::canonicalize(
            std::env::var_os("AFT_TEST_CACHE_VOLUME").expect("set the mounted test volume"),
        )
        .unwrap();
        let on_volume = tempfile::tempdir_in(&volume).unwrap();
        let cache = on_volume.path().join("cache");
        std::fs::create_dir(&cache).unwrap();
        let primary = tempfile::tempdir().unwrap();
        let storage = primary.path().join("storage");
        let mut config = crate::config::Config {
            storage_dir: Some(storage.clone()),
            cache_dir: Some(cache.clone()),
            ..Default::default()
        };
        let warning = crate::root_cache::prepare_cache_root(&mut config);
        assert!(warning.is_none(), "cache root not enrolled: {warning:?}");
        let models = config.semantic.local_cache_dir.clone().unwrap();
        drop(config);
        std::fs::remove_dir_all(&cache).unwrap();
        std::fs::create_dir(&cache).unwrap();

        let error = super::resolve_model_files(Some(&models)).unwrap_err();

        assert!(
            error.starts_with(crate::root_cache::CACHE_DIR_UNAVAILABLE),
            "{error}"
        );
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
        assert!(!storage.join("semantic").exists());
    }

    static NATIVE_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn native_test() -> std::sync::MutexGuard<'static, ()> {
        NATIVE_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn fixture_corpus() -> Vec<String> {
        vec![
            String::new(),
            "A local semantic query about memory and indexing".into(),
            "pub fn checksum(input: u64) -> u64 { input.wrapping_mul(37) }".into(),
            "Café naïve 日本語 🦀".into(),
            "attention mask pooling ".repeat(600),
        ]
    }

    fn bits(vectors: Vec<Vec<f32>>) -> Vec<Vec<u32>> {
        vectors
            .into_iter()
            .map(|v| v.into_iter().map(f32::to_bits).collect())
            .collect()
    }

    #[test]
    #[ignore = "requires cached MiniLM and ONNX Runtime; run explicitly with ORT_DYLIB_PATH"]
    fn local_embedding_vectors_match_legacy_fixture() {
        let _serial = native_test();
        let mut legacy =
            super::NativeEmbedder::new("all-MiniLM-L6-v2", super::InferenceRole::Build).unwrap();
        // The old backend used default run options and kept the CPU arena.
        unsafe { std::mem::ManuallyDrop::drop(&mut legacy.run_options) };
        legacy.run_options = std::mem::ManuallyDrop::new(ort::session::RunOptions::new().unwrap());
        let corpus = fixture_corpus();
        let expected = bits(legacy.embed(&corpus).unwrap());
        let mut bounded = super::LocalEmbedder::new("all-MiniLM-L6-v2").unwrap();
        assert_eq!(bits(bounded.embed(&corpus).unwrap()), expected);
        // Force the same attention-budget split used by long indexing batches.
        let long = vec![corpus.last().unwrap().clone(); 32];
        assert_eq!(
            bits(bounded.embed(&long).unwrap()),
            bits(legacy.embed(&long).unwrap())
        );
    }

    #[test]
    #[ignore = "requires cached MiniLM and ONNX Runtime; run explicitly with ORT_DYLIB_PATH"]
    fn local_embedding_two_sessions_serve_all_roots_and_query_workers() {
        let _serial = native_test();
        let config = crate::config::SemanticBackendConfig::default();
        let mut root_one =
            crate::semantic_index::SemanticEmbeddingModel::from_config(&config).unwrap();
        let mut root_two =
            crate::semantic_index::SemanticEmbeddingModel::from_config(&config).unwrap();
        let mut query =
            crate::semantic_index::SemanticEmbeddingModel::from_config_for_query(&config).unwrap();
        assert_eq!(
            super::session_memory().sessions,
            2,
            "roots loaded more than one session per inference role"
        );
        let corpus = fixture_corpus();
        let expected = bits(root_one.embed(corpus.clone()).unwrap());
        assert_eq!(bits(root_two.embed(corpus.clone()).unwrap()), expected);
        assert_eq!(bits(query.embed(corpus).unwrap()), expected);
        let text = "semantic query worker";
        let build_vector = root_one.embed(vec![text.into()]).unwrap().remove(0);
        let query_vector = query
            .embed_query_cached(
                text,
                crate::semantic_index::QueryBudget::from_config(&config),
            )
            .unwrap();
        assert_eq!(bits(vec![query_vector]), bits(vec![build_vector]));
        assert_eq!(super::session_memory().sessions, 2);
        // Query workers own provider clones until their request channel closes.
        drop((root_one, root_two, query));
        wait_for_sessions(0);
    }

    fn wait_for_sessions(expected: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while super::session_memory().sessions != expected && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(super::session_memory().sessions, expected);
    }

    #[test]
    #[ignore = "requires cached MiniLM and ONNX Runtime; exercises connected-cache reload and recovery"]
    fn local_embedding_connected_cache_survives_idle_reload_and_volume_recovery() {
        let _serial = native_test();
        let (model, tokenizer) = super::resolve_model_files(None).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let volume = temp.path().join("cache-volume");
        std::fs::create_dir(&volume).unwrap();
        let mut config = crate::config::Config {
            storage_dir: Some(temp.path().join("storage")),
            cache_dir: Some(volume.clone()),
            ..Default::default()
        };
        assert!(crate::root_cache::prepare_cache_root_for_test(&mut config).is_none());
        let models = config.semantic.local_cache_dir.clone().unwrap();
        let snapshot = models.join("models--Qdrant--all-MiniLM-L6-v2-onnx/snapshots/fixture");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::copy(model, snapshot.join("model.onnx")).unwrap();
        std::fs::copy(tokenizer, snapshot.join("tokenizer.json")).unwrap();

        let mut query_model =
            crate::semantic_index::SemanticEmbeddingModel::from_config_for_query(&config.semantic)
                .unwrap();
        let mut build_model =
            crate::semantic_index::SemanticEmbeddingModel::from_config(&config.semantic).unwrap();
        let build =
            super::LocalEmbedder::with_cache_dir("all-MiniLM-L6-v2", Some(&models)).unwrap();
        let query =
            super::LocalEmbedder::with_query_cache_dir("all-MiniLM-L6-v2", Some(&models)).unwrap();
        assert!(build
            .shared
            .lock()
            .unwrap()
            .model
            .as_ref()
            .unwrap()
            .model_path
            .starts_with(&models));
        assert!(query
            .shared
            .lock()
            .unwrap()
            .model
            .as_ref()
            .unwrap()
            .model_path
            .starts_with(&models));
        let corpus = vec!["connected cache model reload".to_string()];
        let expected = bits(build_model.embed(corpus.clone()).unwrap());
        assert_eq!(bits(query_model.embed(corpus.clone()).unwrap()), expected);
        assert_eq!(super::session_memory().sessions, 2);
        {
            let mut resident = build.shared.lock().unwrap();
            let expired = resident.last_used + super::MODEL_IDLE_TTL;
            resident.release_if_idle(expired);
        }
        assert_eq!(super::session_memory().sessions, 1);
        drop(config);
        let disconnected = temp.path().join("disconnected-cache");
        std::fs::rename(&volume, &disconnected).unwrap();
        std::fs::create_dir(&volume).unwrap();

        let error = build_model.embed(corpus.clone()).unwrap_err();
        assert!(
            error.contains(crate::root_cache::CACHE_DIR_UNAVAILABLE),
            "{error}"
        );
        assert_eq!(
            std::fs::read_dir(&volume).unwrap().count(),
            0,
            "reload wrote into the replacement volume"
        );
        assert_eq!(bits(query_model.embed(corpus.clone()).unwrap()), expected);

        std::fs::remove_dir(&volume).unwrap();
        std::fs::rename(&disconnected, &volume).unwrap();
        assert_eq!(bits(build_model.embed(corpus).unwrap()), expected);
        assert_eq!(super::session_memory().sessions, 2);
        drop((query_model, build_model, query, build));
        wait_for_sessions(0);
    }

    #[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
    #[test]
    #[ignore = "process-wide native allocation measurement; requires cached MiniLM and ORT"]
    fn local_embedding_cpu_arena_releases_high_water() {
        let _serial = native_test();
        let mut handle = super::LocalEmbedder::new("all-MiniLM-L6-v2").unwrap();
        let allocated = || {
            crate::memory::AllocatorObservation::measure()
                .snapshot
                .bytes_in_use
                .expect("native allocator measurement")
        };
        let before = allocated();
        let corpus = vec!["attention mask pooling ".repeat(600); 15];
        drop(handle.embed(&corpus).unwrap());
        let retained = allocated().saturating_sub(before);
        // The attention budget permits a >1 GB transient allocation. Native
        // live allocations must not keep that high-water mark after the run.
        assert!(
            retained < 256 * 1024 * 1024,
            "CPU arena retained {retained} bytes after inference"
        );
    }

    #[test]
    #[ignore = "requires cached MiniLM and ONNX Runtime; run explicitly with ORT_DYLIB_PATH"]
    fn local_embedding_build_unloads_but_queries_stay_warm() {
        let _serial = native_test();
        let mut handle = super::LocalEmbedder::new("all-minilm-l6-v2").unwrap();
        let mut query = super::LocalEmbedder::new_for_query("all-MiniLM-L6-v2").unwrap();
        let corpus = fixture_corpus();
        let expected = bits(handle.embed(&corpus).unwrap());
        {
            let mut resident = handle.shared.lock().unwrap();
            let last_used = resident.last_used;
            resident.release_if_idle(
                last_used + super::MODEL_IDLE_TTL - std::time::Duration::from_nanos(1),
            );
            assert_eq!(
                super::session_memory().sessions,
                2,
                "released before the idle deadline"
            );
            resident.release_if_idle(last_used + super::MODEL_IDLE_TTL);
        }
        assert_eq!(
            super::session_memory().sessions,
            1,
            "idle build must unload without releasing the warm query session"
        );
        {
            let mut resident = query.shared.lock().unwrap();
            let after_idle = resident.last_used + super::MODEL_IDLE_TTL * 2;
            resident.release_if_idle(after_idle);
            assert!(resident.model.is_some(), "query lease lost its warm model");
        }
        assert_eq!(bits(query.embed(&corpus).unwrap()), expected);
        assert_eq!(bits(handle.embed(&corpus).unwrap()), expected);
        assert_eq!(super::session_memory().sessions, 2);
        drop((handle, query));
        assert_eq!(super::session_memory().sessions, 0);
        assert_eq!(super::session_memory().model_bytes, 0);
    }

    #[test]
    #[ignore = "requires cached MiniLM and ONNX Runtime; run explicitly with ORT_DYLIB_PATH"]
    fn local_embedding_query_completes_with_build_gate_held() {
        let _serial = native_test();
        let build = super::LocalEmbedder::new("all-MiniLM-L6-v2").unwrap();
        // Hold the exact mutex used for a whole indexing inference. Query
        // progress must not depend on releasing this gate or on scheduler luck.
        let build_gate = build.shared.lock().unwrap();
        let query = super::LocalEmbedder::new_for_query("all-MiniLM-L6-v2").unwrap();
        assert!(
            query.shared.try_lock().is_ok(),
            "query inference is blocked by the held build gate"
        );
        let config = crate::config::SemanticBackendConfig {
            query_timeout_ms: 30_000,
            ..Default::default()
        };
        let mut model =
            crate::semantic_index::SemanticEmbeddingModel::from_config_for_query(&config).unwrap();
        assert_eq!(
            model.embed(vec!["query provider".into()]).unwrap()[0].len(),
            384
        );
        let vector = model
            .embed_query_cached(
                "query worker",
                crate::semantic_index::QueryBudget::from_config(&config),
            )
            .unwrap();
        assert_eq!(vector.len(), 384);
        let mut refresh_model =
            crate::semantic_index::SemanticEmbeddingModel::from_config(&config).unwrap();
        let vector = refresh_model
            .embed_query_cached(
                "parent-folder query",
                crate::semantic_index::QueryBudget::from_config(&config),
            )
            .unwrap();
        assert_eq!(vector.len(), 384);
        drop(build_gate);
        drop((model, refresh_model, query, build));
        wait_for_sessions(0);
    }

    #[test]
    #[ignore = "requires cached MiniLM and ONNX Runtime; run explicitly with ORT_DYLIB_PATH"]
    fn local_embedding_root_eviction_unbind_and_census_release_sessions() {
        let _serial = native_test();
        for eviction in [0, 1, 2] {
            let ctx = crate::context::AppContext::new(
                Box::new(crate::parser::TreeSitterProvider::new()),
                crate::config::Config::default(),
            );
            *ctx.semantic_embedding_model().lock() = Some(
                crate::semantic_index::SemanticEmbeddingModel::from_config_for_query(
                    &crate::config::SemanticBackendConfig::default(),
                )
                .unwrap(),
            );
            let snapshot = crate::memory::MemorySnapshot::new("ready", Default::default());
            assert_eq!(snapshot.process.embedding_sessions, 1);
            assert!(snapshot.process.embedding_model_bytes > 80_000_000);
            assert!(
                snapshot.process.total_attributed_bytes >= snapshot.process.embedding_model_bytes
            );
            match eviction {
                0 => assert!(ctx.evict_idle_artifacts()),
                1 => assert!(ctx.try_evict_idle_artifacts()),
                _ => ctx.cancel_unbound_artifact_work(),
            }
            assert!(ctx.semantic_embedding_model().lock().is_none());
            wait_for_sessions(0);
            let snapshot = crate::memory::MemorySnapshot::new("ready", Default::default());
            assert_eq!(snapshot.process.embedding_sessions, 0);
            assert_eq!(snapshot.process.embedding_model_bytes, 0);
        }
    }

    #[test]
    fn empty_fastembed_and_home_rungs_are_unset_with_an_injected_lookup() {
        let empty = std::ffi::OsString::new();
        let cache = embedding_cache_dir_from(
            |name| match name {
                "FASTEMBED_CACHE_DIR" | "HOME" => Some(empty.clone()),
                "USERPROFILE" => Some(std::ffi::OsString::from("/profile")),
                _ => None,
            },
            None,
        );
        assert_eq!(
            cache,
            Some(std::path::PathBuf::from("/profile/.cache/fastembed"))
        );
        assert_eq!(embedding_cache_dir_from(|_| None, None), None);
    }

    #[test]
    fn absolute_xdg_cache_home_outranks_home_and_a_relative_one_is_ignored() {
        let xdg = if cfg!(windows) {
            "C:\\xdg-cache"
        } else {
            "/xdg-cache"
        };
        let lookup = |xdg_value: &'static str| {
            move |name: &str| match name {
                "XDG_CACHE_HOME" => Some(std::ffi::OsString::from(xdg_value)),
                "HOME" => Some(std::ffi::OsString::from("/home/user")),
                _ => None,
            }
        };
        assert_eq!(
            embedding_cache_dir_from(lookup(xdg), None),
            Some(std::path::Path::new(xdg).join("fastembed"))
        );
        assert_eq!(
            embedding_cache_dir_from(lookup("relative-cache"), None),
            Some(std::path::PathBuf::from("/home/user/.cache/fastembed"))
        );
        assert_eq!(
            embedding_cache_dir_from(
                |name| match name {
                    "FASTEMBED_CACHE_DIR" => Some(std::ffi::OsString::from("/models")),
                    "XDG_CACHE_HOME" => Some(std::ffi::OsString::from(xdg)),
                    _ => None,
                },
                None,
            ),
            Some(std::path::PathBuf::from("/models")),
            "an explicit FASTEMBED_CACHE_DIR still wins"
        );
    }

    fn minilm_like_tokenizer_json() -> Vec<u8> {
        serde_json::json!({
            "version": "1.0",
            "truncation": {
                "direction": "Right",
                "max_length": MINILM_MAX_LENGTH,
                "strategy": "LongestFirst",
                "stride": 0
            },
            "padding": null,
            "added_tokens": [
                {"id": 0, "content": "[PAD]", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true},
                {"id": 1, "content": "[CLS]", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true},
                {"id": 2, "content": "[SEP]", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true},
                {"id": 3, "content": "[UNK]", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true}
            ],
            "normalizer": {
                "type": "BertNormalizer",
                "clean_text": true,
                "handle_chinese_chars": true,
                "strip_accents": null,
                "lowercase": true
            },
            "pre_tokenizer": {"type": "BertPreTokenizer"},
            "post_processor": {"type": "BertProcessing", "sep": ["[SEP]", 2], "cls": ["[CLS]", 1]},
            "decoder": null,
            "model": {
                "type": "WordPiece",
                "unk_token": "[UNK]",
                "continuing_subword_prefix": "##",
                "max_input_chars_per_word": 100,
                "vocab": {
                    "[PAD]": 0,
                    "[CLS]": 1,
                    "[SEP]": 2,
                    "[UNK]": 3,
                    "hello": 4,
                    "world": 5,
                    "!": 6,
                    "cafe": 7,
                    "naive": 8,
                    "##ly": 9
                }
            }
        })
        .to_string()
        .into_bytes()
    }

    fn assert_load_encode_parity(tokenizer: Tokenizer) {
        let ascii = tokenizer.encode("Hello WORLD!", true).unwrap();
        assert_eq!(ascii.get_ids(), &[1, 4, 5, 6, 2]);

        let unicode = tokenizer.encode("Café naïvely", true).unwrap();
        assert_eq!(unicode.get_ids(), &[1, 7, 8, 9, 2]);

        let long_text = std::iter::repeat("hello")
            .take(MINILM_MAX_LENGTH + 20)
            .collect::<Vec<_>>()
            .join(" ");
        let long = tokenizer.encode(long_text.as_str(), true).unwrap();
        let ids = long.get_ids();
        assert_eq!(ids.len(), MINILM_MAX_LENGTH);
        assert_eq!(ids.first(), Some(&1));
        assert_eq!(ids.last(), Some(&2));
        assert!(ids[1..MINILM_MAX_LENGTH - 1].iter().all(|id| *id == 4));
    }

    #[test]
    fn cgroup_cpu_quota_parsing_and_thread_derivation_cover_v2_v1_and_absence() {
        assert_eq!(
            parse_cgroup_v2_cpu_max("max 100000\n"),
            CgroupCpuQuota::Unlimited
        );
        assert_eq!(
            parse_cgroup_v2_cpu_max("200000 100000\n"),
            CgroupCpuQuota::Limited(2)
        );
        assert_eq!(
            parse_cgroup_v1_cpu_quota("-1\n", "100000\n"),
            CgroupCpuQuota::Unlimited
        );
        assert_eq!(
            parse_cgroup_v1_cpu_quota("200000\n", "100000\n"),
            CgroupCpuQuota::Limited(2)
        );

        let v2_limited = derive_intra_threads(64, Some("200000 100000"), None, None);
        assert_eq!(v2_limited.threads, 2);
        assert_eq!(v2_limited.source, "quota");

        let v1_limited = derive_intra_threads(64, None, Some("200000"), Some("100000"));
        assert_eq!(v1_limited.threads, 2);
        assert_eq!(v1_limited.source, "quota");

        let v2_unlimited = derive_intra_threads(64, Some("max 100000"), None, None);
        assert_eq!(v2_unlimited.threads, 8);
        assert_eq!(v2_unlimited.source, "cap");
        assert_eq!(v2_unlimited.quota_threads, None);

        let v1_unlimited = derive_intra_threads(64, None, Some("-1"), Some("100000"));
        assert_eq!(v1_unlimited.threads, 8);
        assert_eq!(v1_unlimited.source, "cap");
        assert_eq!(v1_unlimited.quota_threads, None);

        let absent = derive_intra_threads(64, None, None, None);
        assert_eq!(absent.threads, 8);
        assert_eq!(absent.source, "cap");
        assert_eq!(absent.quota_threads, None);
    }

    #[test]
    fn tokenizers_slim_features_load_and_encode_minilm_wordpiece() {
        let json = minilm_like_tokenizer_json();

        assert_load_encode_parity(Tokenizer::from_bytes(&json).unwrap());

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&json).unwrap();
        file.flush().unwrap();
        assert_load_encode_parity(Tokenizer::from_file(file.path()).unwrap());
    }
}
