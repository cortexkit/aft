//! Recorded reranker scores for the deterministic search-quality gate.
//!
//! A pack stores one score per (model fingerprint, prose query, candidate-text
//! hash). The fixture backend answers only from the pack, so the gate is
//! byte-identical on every platform and needs no model. A score missing from
//! the pack is a hard failure: falling back to fused order would quietly
//! measure the engine without the reranker. The recorder wraps a live backend
//! (the ONNX one) and appends every score it produces to a pack.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{RerankBackend, RerankDoc, RerankError, RerankFingerprint, TEXT_POLICY_REVISION};

pub(crate) const PACK_VERSION: u32 = 1;

/// The fingerprint of the backend whose scores a pack holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PackFingerprint {
    pub(crate) backend: String,
    pub(crate) model: String,
    pub(crate) revision: String,
}

impl From<&RerankFingerprint> for PackFingerprint {
    fn from(fingerprint: &RerankFingerprint) -> Self {
        Self {
            backend: fingerprint.backend.to_string(),
            model: fingerprint.model.clone(),
            revision: fingerprint.revision.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PackScore {
    pub(crate) query: String,
    pub(crate) text_sha256: String,
    pub(crate) score: f32,
}

/// On-disk pack. Scores are kept sorted by (query, text hash) so a re-record
/// of the same inputs produces the same bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct FixturePack {
    pub(crate) version: u32,
    pub(crate) fingerprint: PackFingerprint,
    pub(crate) text_policy: String,
    pub(crate) scores: Vec<PackScore>,
}

impl FixturePack {
    pub(crate) fn empty(fingerprint: &RerankFingerprint) -> Self {
        Self {
            version: PACK_VERSION,
            fingerprint: fingerprint.into(),
            text_policy: TEXT_POLICY_REVISION.to_string(),
            scores: Vec::new(),
        }
    }

    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        super::note_io();
        let bytes = std::fs::read(path)
            .map_err(|error| format!("read rerank pack {}: {error}", path.display()))?;
        let pack: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse rerank pack {}: {error}", path.display()))?;
        if pack.version != PACK_VERSION {
            return Err(format!(
                "rerank pack {} has version {}, expected {PACK_VERSION}",
                path.display(),
                pack.version
            ));
        }
        if pack.text_policy != TEXT_POLICY_REVISION {
            return Err(format!(
                "rerank pack {} was recorded with candidate text {:?}, the engine builds {TEXT_POLICY_REVISION:?}; re-record it",
                path.display(),
                pack.text_policy
            ));
        }
        Ok(pack)
    }

    pub(crate) fn save(&self, path: &Path) -> Result<(), String> {
        let mut bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("serialize rerank pack: {error}"))?;
        bytes.push(b'\n');
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, &bytes)
            .map_err(|error| format!("write rerank pack {}: {error}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .map_err(|error| format!("replace rerank pack {}: {error}", path.display()))
    }

    /// Add or replace scores, keeping the list sorted and duplicate-free.
    pub(crate) fn merge(&mut self, recorded: impl IntoIterator<Item = PackScore>) {
        let mut by_key = self
            .scores
            .drain(..)
            .map(|score| ((score.query.clone(), score.text_sha256.clone()), score))
            .collect::<BTreeMap<_, _>>();
        for score in recorded {
            by_key.insert((score.query.clone(), score.text_sha256.clone()), score);
        }
        self.scores = by_key.into_values().collect();
    }
}

pub(crate) fn text_sha256(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Serves scores from a pack and nothing else.
pub(crate) struct FixtureBackend {
    fingerprint: RerankFingerprint,
    scores: HashMap<(String, String), f32>,
}

impl FixtureBackend {
    pub(crate) fn from_pack(pack: &FixturePack) -> Self {
        Self {
            // The recorded backend and model stay visible in the fingerprint,
            // so an order memoized from a pack never collides with a live one.
            fingerprint: RerankFingerprint {
                backend: "fixture",
                model: format!("{}:{}", pack.fingerprint.backend, pack.fingerprint.model),
                revision: pack.fingerprint.revision.clone(),
            },
            scores: pack
                .scores
                .iter()
                .map(|score| {
                    (
                        (score.query.clone(), score.text_sha256.clone()),
                        score.score,
                    )
                })
                .collect(),
        }
    }
}

impl RerankBackend for FixtureBackend {
    fn fingerprint(&self) -> RerankFingerprint {
        self.fingerprint.clone()
    }

    fn max_batch(&self) -> usize {
        usize::MAX
    }

    fn score(
        &self,
        query: &str,
        docs: &[RerankDoc<'_>],
        _deadline: Instant,
    ) -> Result<Vec<f32>, RerankError> {
        docs.iter()
            .map(|doc| {
                let hash = text_sha256(doc.text);
                self.scores
                    .get(&(query.to_string(), hash.clone()))
                    .copied()
                    .ok_or_else(|| {
                        RerankError::Failed(format!(
                            "no recorded score for query {query:?} and text {hash}"
                        ))
                    })
            })
            .collect()
    }
}

/// The fixture backend for a pack path, loaded once per process.
pub(crate) fn shared_fixture_backend(path: &Path) -> Result<Arc<dyn RerankBackend>, String> {
    static LOADED: OnceLock<Mutex<HashMap<PathBuf, Arc<FixtureBackend>>>> = OnceLock::new();
    let mut loaded = LOADED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(backend) = loaded.get(path) {
        return Ok(backend.clone());
    }
    let backend = Arc::new(FixtureBackend::from_pack(&FixturePack::load(path)?));
    loaded.insert(path.to_path_buf(), backend.clone());
    Ok(backend)
}

/// Wraps a live backend and appends every score it returns to a pack file.
pub(crate) struct RecordingBackend {
    inner: Arc<dyn RerankBackend>,
    pack_path: PathBuf,
    write_lock: Mutex<()>,
}

pub(crate) fn recording(
    inner: Arc<dyn RerankBackend>,
    pack_path: PathBuf,
) -> Arc<dyn RerankBackend> {
    static RECORDERS: OnceLock<
        Mutex<HashMap<(PathBuf, RerankFingerprint), Arc<RecordingBackend>>>,
    > = OnceLock::new();
    let key = (pack_path.clone(), inner.fingerprint());
    let mut recorders = RECORDERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    recorders
        .entry(key)
        .or_insert_with(|| {
            Arc::new(RecordingBackend {
                inner,
                pack_path,
                write_lock: Mutex::new(()),
            })
        })
        .clone()
}

impl RecordingBackend {
    fn record(&self, query: &str, docs: &[RerankDoc<'_>], scores: &[f32]) -> Result<(), String> {
        super::note_io();
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fingerprint = self.inner.fingerprint();
        let mut pack = if self.pack_path.exists() {
            FixturePack::load(&self.pack_path)?
        } else {
            FixturePack::empty(&fingerprint)
        };
        if pack.fingerprint != PackFingerprint::from(&fingerprint) {
            return Err(format!(
                "rerank pack {} holds scores for {:?}, not {:?}",
                self.pack_path.display(),
                pack.fingerprint,
                PackFingerprint::from(&fingerprint)
            ));
        }
        pack.merge(docs.iter().zip(scores).map(|(doc, score)| PackScore {
            query: query.to_string(),
            text_sha256: text_sha256(doc.text),
            score: *score,
        }));
        pack.save(&self.pack_path)
    }
}

impl RerankBackend for RecordingBackend {
    fn fingerprint(&self) -> RerankFingerprint {
        self.inner.fingerprint()
    }

    fn max_batch(&self) -> usize {
        self.inner.max_batch()
    }

    fn score(
        &self,
        query: &str,
        docs: &[RerankDoc<'_>],
        deadline: Instant,
    ) -> Result<Vec<f32>, RerankError> {
        let scores = self.inner.score(query, docs, deadline)?;
        if let Err(error) = self.record(query, docs, &scores) {
            crate::slog_warn!("rerank recorder: {error}");
        }
        Ok(scores)
    }
}
