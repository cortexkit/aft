//! Local ONNX embedding backend (all-MiniLM-L6-v2) driven directly through
//! `ort`.
//!
//! Replaces the `fastembed` crate. We own the ORT session so we can cap
//! intra-op threads — `fastembed` hardcoded `with_intra_threads(all cores)`,
//! which pegged every core during indexing (the sustained-CPU complaint). We
//! cap to `num_cpus / 2`, which an earlier measurement showed is both faster
//! (1.7x) and far lighter (3.5x less CPU) than oversubscribing all cores.
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

use std::path::{Path, PathBuf};

use ort::session::builder::GraphOptimizationLevel;
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
/// count (ORT's arena grows to the high-water mark and stays there). Measured:
/// `64 × 512² = 16.78M units → ~4.92 GB peak` — too high for 8–16 GB machines.
///
/// 4.0M units caps the worst case at roughly half that (~2–2.5 GB, re-measured):
/// at 512-token chunks it allows ~15 per inference; at ≤250 tokens (the common
/// case for code symbols) it allows the full 64-chunk batch, so short-chunk
/// throughput is unaffected and only long-chunk batches are split.
const MAX_BATCH_ATTENTION_UNITS: usize = 4_000_000;

/// Cap ORT intra-op threads to half the cores (min 1), leaving the rest free
/// for the agent / editor. Matches the `num_cpus / 2` policy used elsewhere.
fn intra_thread_cap() -> usize {
    std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1)
        .div_ceil(2)
        .max(1)
}

pub struct LocalEmbedder {
    session: Session,
    tokenizer: Tokenizer,
    wants_token_type_ids: bool,
}

impl LocalEmbedder {
    /// Build the embedder for the named model. Only `all-MiniLM-L6-v2` is
    /// supported as the local backend (matches the prior fastembed surface).
    pub fn new(model: &str) -> Result<Self, String> {
        match model {
            "all-MiniLM-L6-v2" | "all-minilm-l6-v2" => {}
            other => {
                return Err(format!(
                    "unsupported local embedding model '{other}'. Supported: all-MiniLM-L6-v2"
                ))
            }
        }

        // Fail with an actionable message instead of letting ort panic deep
        // inside dlopen on an incompatible/absent ONNX Runtime.
        pre_validate_onnx_runtime()?;

        let (model_path, tokenizer_path) = resolve_model_files()?;

        let threads = intra_thread_cap();
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
            "local embedder ready: model=all-MiniLM-L6-v2 intra_threads={} token_type_ids={}",
            threads,
            wants_token_type_ids
        );

        Ok(Self {
            session,
            tokenizer,
            wants_token_type_ids,
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

        let text_refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let encodings = self
            .tokenizer
            .encode_batch(text_refs, true)
            .map_err(|e| format!("tokenize batch: {e}"))?;

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
            .run(inputs)
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

#[inline]
fn mask_at(mask: &ndarray::Array2<i64>, row: usize, col: usize) -> i64 {
    mask[[row, col]]
}

/// Resolve the MiniLM model.onnx + tokenizer.json, reusing an existing local
/// download when present (offline-safe) and falling back to an hf-hub fetch.
fn resolve_model_files() -> Result<(PathBuf, PathBuf), String> {
    let cache_dir = embedding_cache_dir()?;

    if let Some(found) = scan_local_snapshot(&cache_dir) {
        return Ok(found);
    }

    // Not cached locally — download via hf-hub into the same cache layout so a
    // subsequent run finds it through the local scan above.
    download_via_hf_hub(&cache_dir)
}

/// fastembed read `FASTEMBED_CACHE_DIR`; the bridge/warmup set it to
/// `<storage>/semantic/models`. Keep the same env + default so existing
/// downloads are reused.
fn embedding_cache_dir() -> Result<PathBuf, String> {
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
    use super::{embedding_cache_dir_from, MINILM_MAX_LENGTH};
    use std::io::Write;
    use tokenizers::Tokenizer;

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
    fn tokenizers_slim_features_load_and_encode_minilm_wordpiece() {
        let json = minilm_like_tokenizer_json();

        assert_load_encode_parity(Tokenizer::from_bytes(&json).unwrap());

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&json).unwrap();
        file.flush().unwrap();
        assert_load_encode_parity(Tokenizer::from_file(file.path()).unwrap());
    }
}
