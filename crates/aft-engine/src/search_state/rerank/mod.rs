//! Reranker backend contracts, configuration and per-context installation state.

pub(crate) mod fixture;
pub(crate) mod onnx;
pub(crate) mod remote;
pub(crate) mod slot;
pub(crate) mod synapse;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{RerankBackendKind, SearchConfig};

/// How many entries at the head of the canonical list are reranked when the
/// config does not say. Chosen so a cross-encoder answers within the default
/// deadline on a laptop CPU; entries below it keep their fused order.
pub(crate) const DEFAULT_TOP_N: usize = 20;
/// The reranked prefix never reaches past the first block of the canonical
/// list. That block holds every candidate found within the shallowest
/// retrieval depth, which every request observes, so its contents are the same
/// whatever `topK` and `offset` a request asks for.
pub(crate) const MAX_TOP_N: usize = super::BLOCK_DEPTHS[0];
/// Per-call scoring budget when the config does not set one.
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 1_500;
pub(crate) const MIN_TIMEOUT_MS: u64 = 50;
pub(crate) const MAX_TIMEOUT_MS: u64 = 15_000;
/// Names the candidate-text construction used by the search tool. Bump it
/// whenever that construction changes, so memoized orders and recorded fixture
/// scores made with the old text are never reused.
pub(crate) const TEXT_POLICY_REVISION: &str = "path:line+name+lines/v2";
/// Longest skip reason shown to the agent.
const MAX_REASON_CHARS: usize = 80;

/// Process environment variable naming a recorded score pack. When set, the
/// fixture backend replaces whatever the config selects, and a score missing
/// from the pack fails the search instead of silently keeping fused order.
pub(crate) const FIXTURE_PACK_ENV: &str = "AFT_RERANK_FIXTURE_PACK";
/// Process environment variable naming a pack file that the ONNX backend's
/// scores are appended to while serving normally.
pub(crate) const RECORD_PACK_ENV: &str = "AFT_RERANK_RECORD_PACK";

/// One candidate document handed to a backend: the bounded candidate text.
pub(crate) struct RerankDoc<'a> {
    pub(crate) text: &'a str,
}

/// Names the exact scoring function a backend applies. Two backends with equal
/// fingerprints must produce equal scores for equal inputs, so the fingerprint
/// is part of every memoized ordering and every recorded fixture score.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RerankFingerprint {
    pub(crate) backend: &'static str,
    pub(crate) model: String,
    pub(crate) revision: String,
}

/// Why a backend returned no scores. None of these is a search error: the
/// caller keeps the fused order and reports one short note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RerankError {
    /// The backend cannot serve now (model not provisioned yet, runtime
    /// missing, worker busy, process shutting down).
    Unavailable(String),
    /// No answer arrived before the caller's deadline.
    Timeout,
    /// The backend rejected the request (bad input, over a size limit).
    Refused(String),
    /// Scoring started and failed.
    Failed(String),
}

impl RerankError {
    pub(crate) fn skip_reason(&self) -> String {
        let reason = match self {
            Self::Unavailable(reason) => reason.clone(),
            Self::Timeout => "timeout".to_string(),
            Self::Refused(reason) => format!("refused: {reason}"),
            Self::Failed(reason) => format!("failed: {reason}"),
        };
        short_reason(&reason)
    }
}

pub(crate) trait RerankBackend: Send + Sync {
    /// The identity of the scoring function. It must be constant for the
    /// lifetime of the instance and must do no I/O: it is read on the search
    /// path and is part of the key under which a list's rerank outcome is
    /// committed, so a value that changed mid-paging would rerank a list whose
    /// earlier page was already served in another order. A backend that needs
    /// I/O to learn its identity (asking a server which model it serves) must
    /// learn it while it is being built, off the search path; until it is
    /// built no backend is installed, which counts as reranking off.
    fn fingerprint(&self) -> RerankFingerprint;
    fn max_batch(&self) -> usize;
    /// One score per doc, same order; must return by `deadline` or Err(Timeout).
    fn score(
        &self,
        query: &str,
        docs: &[RerankDoc<'_>],
        deadline: Instant,
    ) -> Result<Vec<f32>, RerankError>;
}

/// A backend that scores every document zero, so a stable reorder keeps the
/// prior order. Tests use it where a backend must exist but must not move
/// anything.
#[cfg(test)]
pub(crate) struct NoopRerankBackend;

#[cfg(test)]
impl RerankBackend for NoopRerankBackend {
    fn fingerprint(&self) -> RerankFingerprint {
        RerankFingerprint {
            backend: "noop",
            model: "noop".to_string(),
            revision: "0".to_string(),
        }
    }

    fn max_batch(&self) -> usize {
        usize::MAX
    }

    fn score(
        &self,
        _query: &str,
        docs: &[RerankDoc<'_>],
        _deadline: Instant,
    ) -> Result<Vec<f32>, RerankError> {
        Ok(vec![0.0; docs.len()])
    }
}

/// The `search.rerank` block after defaults and clamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RerankSettings {
    pub(crate) backend: RerankBackendKind,
    pub(crate) model: Option<String>,
    pub(crate) top_n: usize,
    pub(crate) timeout: Duration,
}

impl RerankSettings {
    pub(crate) fn resolve(search: &SearchConfig) -> Self {
        let rerank = search.rerank.clone().unwrap_or_default();
        let top_n = rerank
            .top_n
            .map_or(DEFAULT_TOP_N, |value| {
                usize::try_from(value).unwrap_or(usize::MAX)
            })
            .clamp(1, MAX_TOP_N);
        let timeout_ms = rerank
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
        Self {
            backend: rerank.backend.unwrap_or(RerankBackendKind::Off),
            model: rerank.model,
            top_n,
            timeout: Duration::from_millis(timeout_ms),
        }
    }
}

/// A backend chosen for one request. `fail_closed` backends turn every
/// scoring failure into a search error. Only the recorded-score fixture is one:
/// it exists so the search-quality benchmark measures ranking with the
/// reranker, and a silent fallback to fused order would make that benchmark
/// measure the engine without it.
#[derive(Clone)]
pub(crate) struct SelectedBackend {
    pub(crate) backend: Arc<dyn RerankBackend>,
    pub(crate) fail_closed: bool,
}

pub(crate) fn short_reason(reason: &str) -> String {
    let line = reason.lines().next().unwrap_or_default().trim();
    match line.char_indices().nth(MAX_REASON_CHARS) {
        None => line.to_string(),
        Some((cut, _)) => format!("{}…", &line[..cut]),
    }
}

#[cfg(test)]
thread_local! {
    static IO_ON_THIS_THREAD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn io_on_this_thread() -> usize {
    IO_ON_THIS_THREAD.with(std::cell::Cell::get)
}

/// Marks a function that reads or writes files or the network. Tests count
/// the marks on their own thread to prove the search path's backend
/// selection never reaches one; in normal builds it does nothing.
#[inline]
pub(crate) fn note_io() {
    #[cfg(test)]
    IO_ON_THIS_THREAD.with(|count| count.set(count.get() + 1));
}
