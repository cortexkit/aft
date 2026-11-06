//! aft_search's semantic lane, read from the data layer.
//!
//! The lane asks the meaning index for the chunks closest to a query
//! vector among the files a request includes, and for the vector length the
//! query must have. [`MeaningIndex`] answers from either source: today's
//! [`SemanticIndex`], or a data-layer meaning plane pinned at one generation
//! ([`DataMeaning`]). The plane returns today's rows over the same vectors
//! (see [`crate::data_layer::vectors`]), so the lane ranks the same chunks
//! the same way from either source.
//!
//! With the data layer on and semantic search switched on for the root,
//! [`DataSemantic`] says how the plane stands, from its own fill rather than
//! today's index status: ready only once every file it applies to is
//! filled, partial (it serves what is filled and the reply says what is
//! missing) while a fill is under way, and building while nothing is filled.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::context::{AppContext, SemanticIndexStatus};
use crate::data_layer::meaning::{self, FillFailure, MeaningState};
use crate::data_layer::vectors::{Coverage, MeaningSnapshot};
use crate::semantic_index::{SemanticIndex, SemanticResult, SemanticSkipReason};

/// The stage a meaning plane reports while its checkout has published
/// nothing yet, so there is no view to fill.
pub(crate) const DATA_LAYER_BUILD_STAGE: &str = "data_layer_build";

/// A data-layer meaning plane pinned at one generation of the family's
/// vectors for one view, with what the last fill reported.
#[derive(Debug)]
pub(crate) struct DataMeaning {
    snapshot: MeaningSnapshot,
    /// The last fill's failure, kept until a fill succeeds.
    failure: Option<FillFailure>,
}

impl DataMeaning {
    /// Names what the lane's answers come from: the view's text generation
    /// and the generation of the vectors. A recorded change and a fill that
    /// lands each change it, so a list cached under it is never served over
    /// other vectors.
    pub(crate) fn generation(&self) -> String {
        format!(
            "{}:vectors:{}",
            self.snapshot.view_generation(),
            self.snapshot.generation()
        )
    }

    /// Logs, at debug level, that a ranking read this plane. The data-layer
    /// parity harness (`benchmarks/aft-search/compare_data_layer.py`) counts
    /// these lines to show which source answered each request.
    pub(crate) fn note_read(&self, request_id: &str) {
        crate::slog_debug!(
            "aft_search {request_id} read the data layer meaning plane at {}",
            self.generation()
        );
    }

    /// Whether every file meaning search applies to within
    /// `semantic.max_files` has its vectors, or a recorded reason it has
    /// none (too large, or it could not be cut into chunks).
    pub(crate) fn complete(&self) -> bool {
        !meaning::waiting(self.snapshot.coverage())
    }

    pub(crate) fn coverage(&self) -> &Coverage {
        self.snapshot.coverage()
    }

    /// Why the last fill stopped, while files still wait for vectors.
    pub(crate) fn failure(&self) -> Option<&str> {
        self.failure.as_ref().map(FillFailure::message)
    }

    /// The vector length a query needs, once the plane holds any vector;
    /// `None` before, as today's index gives none while it is empty.
    pub(crate) fn dimension(&self) -> Option<usize> {
        (self.snapshot.chunks() > 0).then(|| self.snapshot.dimensions())
    }

    /// The summary today's index gives for chunks the embedder refused, in
    /// the same words, with how many there are and why; `None` when the
    /// embedder refused none.
    pub(crate) fn skipped_chunks(&self) -> Option<(String, usize, SemanticSkipReason)> {
        let root = self.snapshot.root();
        let failed = &self.snapshot.coverage().chunks_failed;
        if failed.is_empty() {
            return None;
        }
        let mut refused = std::collections::BTreeMap::<PathBuf, usize>::new();
        let mut exceeded = std::collections::BTreeMap::<PathBuf, usize>::new();
        for (path, _, reason) in failed {
            let counts = if reason == crate::semantic_index::PROVIDER_REFUSAL_REASON {
                &mut refused
            } else {
                &mut exceeded
            };
            *counts.entry(path.clone()).or_default() += 1;
        }
        let summary = [
            crate::semantic_index::format_skipped_chunks_summary(
                root,
                &refused,
                "the embedding provider refused them",
            ),
            crate::semantic_index::format_skipped_chunks_summary(
                root,
                &exceeded,
                "they exceed the model's input limit",
            ),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
        let reason = if refused.is_empty() {
            SemanticSkipReason::InputExceededModelContext
        } else {
            SemanticSkipReason::ProviderRefusedInput
        };
        Some((summary, failed.len(), reason))
    }
}

/// How this root's meaning search stands on the data layer.
#[derive(Debug, Clone)]
pub(crate) enum DataSemantic {
    /// The plane serves: ready, or partial while a fill is under way (see
    /// [`DataMeaning::complete`]).
    Serving(Arc<DataMeaning>),
    /// The plane cannot serve yet, with the status to report.
    NotServing(SemanticIndexStatus),
}

impl DataSemantic {
    /// This root's meaning search, with the data layer on and semantic
    /// search switched on and admitted for the root: binds the checkout if
    /// nothing has, and starts a fill when files wait for vectors. `None`
    /// otherwise (the root has no checkout in the data layer, or semantic
    /// search is off), and the caller keeps reading today's index.
    pub(crate) fn current(ctx: &AppContext) -> Option<Self> {
        if !crate::data_layer::runtime::enabled() || !ctx.config().indexes.semantic {
            return None;
        }
        let checkout = ctx.data_checkout()?;
        Self::from_state(checkout.meaning(|| ctx.data_meaning_config()))
    }

    /// As [`Self::current`] for status reads: never binds a checkout and
    /// never starts a fill.
    pub(crate) fn probe(ctx: &AppContext) -> Option<Self> {
        use crate::data_layer::surfaces::DataCheckoutProbe;
        if !crate::data_layer::runtime::enabled() || !ctx.config().indexes.semantic {
            return None;
        }
        match ctx.probe_data_checkout()? {
            DataCheckoutProbe::Bound(checkout) => Self::of_checkout(&checkout),
            _ => None,
        }
    }

    /// How `checkout`'s meaning search stands, as its last configuration
    /// left it; starts nothing. `None` while semantic search is off for it.
    pub(crate) fn of_checkout(checkout: &crate::data_layer::runtime::DataCheckout) -> Option<Self> {
        Self::from_state(checkout.meaning_status())
    }

    fn from_state(state: MeaningState) -> Option<Self> {
        Some(match state {
            MeaningState::Off => return None,
            MeaningState::Unpublished => Self::NotServing(building(DATA_LAYER_BUILD_STAGE, None)),
            MeaningState::Starting => Self::NotServing(building(meaning::STARTING_STAGE, None)),
            MeaningState::Blocked(failure) => Self::NotServing(blocked(&failure, None)),
            MeaningState::Serving { snapshot, failure } => {
                let meaning = DataMeaning { snapshot, failure };
                if meaning.complete() || meaning.snapshot.chunks() > 0 {
                    Self::Serving(Arc::new(meaning))
                } else {
                    let coverage = Some(meaning.snapshot.coverage());
                    Self::NotServing(match &meaning.failure {
                        Some(failure) => blocked(failure, coverage),
                        None => building(meaning::FILLING_STAGE, coverage),
                    })
                }
            }
        })
    }

    /// The status aft_search reports for the lane: ready whenever the plane
    /// serves (a partial answer says so itself), otherwise why it does not.
    pub(crate) fn status(&self) -> SemanticIndexStatus {
        match self {
            Self::Serving(_) => SemanticIndexStatus::ready(),
            Self::NotServing(status) => status.clone(),
        }
    }

    pub(crate) fn serving(&self) -> Option<&Arc<DataMeaning>> {
        match self {
            Self::Serving(meaning) => Some(meaning),
            Self::NotServing(_) => None,
        }
    }
}

/// A plane that is building at `stage`, with how far it has come when known.
fn building(stage: &str, coverage: Option<&Coverage>) -> SemanticIndexStatus {
    SemanticIndexStatus::Building {
        stage: stage.to_string(),
        files: coverage.map(|coverage| coverage.files),
        entries_done: coverage.map(files_accounted),
        entries_total: coverage.map(|coverage| coverage.files.saturating_sub(coverage.past_cap)),
    }
}

/// A plane with nothing to serve after `failure`, in the status today's
/// semantic build reports for the same failure: building while it waits for
/// the backend or for ONNX Runtime (it is retried), failed otherwise.
fn blocked(failure: &FillFailure, coverage: Option<&Coverage>) -> SemanticIndexStatus {
    match failure {
        FillFailure::Unreachable(reason) => building(
            &format!("waiting_for_embedding_backend: {reason}"),
            coverage,
        ),
        FillFailure::WaitingForRuntime => building(
            crate::semantic_index::WAITING_FOR_ONNX_RUNTIME_DOWNLOAD_STAGE,
            coverage,
        ),
        FillFailure::Failed(message) => SemanticIndexStatus::Failed(message.clone()),
    }
}

/// Files the plane has accounted for: filled, too large, or failed with a
/// recorded reason.
fn files_accounted(coverage: &Coverage) -> usize {
    coverage.files_embedded + coverage.files_too_large + coverage.files_failed.len()
}

/// Where aft_search's semantic lane reads chunk vectors from.
#[derive(Clone, Copy)]
pub(crate) enum MeaningIndex<'a> {
    /// Today's semantic index.
    Snapshot(&'a SemanticIndex),
    /// The data layer's meaning plane.
    Data(&'a DataMeaning),
}

impl MeaningIndex<'_> {
    /// The `top_k` chunks closest to `query` among files whose absolute
    /// path `include` accepts, best first: `SemanticIndex::search_filtered`.
    ///
    /// # Errors
    ///
    /// When the data layer's store cannot be read; today's index never
    /// fails.
    pub(crate) fn search_filtered(
        &self,
        query: &[f32],
        top_k: usize,
        include: &dyn Fn(&Path) -> bool,
    ) -> io::Result<Vec<SemanticResult>> {
        match self {
            Self::Snapshot(index) => Ok(index.search_filtered(query, top_k, include)),
            Self::Data(meaning) => meaning
                .snapshot
                .search(query, top_k, include)
                .map_err(|error| io::Error::other(error.to_string())),
        }
    }

    /// The vector length a query must have; `None` while the index holds no
    /// vector, when any length is let through.
    pub(crate) fn dimension(&self) -> Option<usize> {
        match self {
            Self::Snapshot(index) => (index.len() > 0).then(|| index.dimension()),
            Self::Data(meaning) => meaning.dimension(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use serde_json::{json, Value};

    use super::*;
    use crate::config::{SemanticBackend, SemanticBackendConfig};
    use crate::protocol::RawRequest;

    const DIMENSIONS: usize = 8;

    /// Every text's vector follows from its digest.
    fn vector_of(text: &str) -> Vec<f32> {
        let hash = blake3::hash(text.as_bytes());
        (0..DIMENSIONS)
            .map(|i| (f32::from(hash.as_bytes()[i]) - 127.5) / 50.0)
            .collect()
    }

    /// Holds every embedding request whose input names `marker` until
    /// [`Gate::open`] is called.
    struct Gate {
        marker: &'static str,
        open: Mutex<bool>,
        opened: Condvar,
        held: AtomicUsize,
        /// Texts embedded, the fingerprint probe aside.
        texts: AtomicUsize,
    }

    impl Gate {
        fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.opened.notify_all();
        }

        fn wait_if_held(&self, texts: &[String]) {
            if !texts.iter().any(|text| text.contains(self.marker)) {
                return;
            }
            self.held.fetch_add(1, Ordering::SeqCst);
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.opened.wait(open).unwrap();
            }
        }
    }

    /// A loopback OpenAI-compatible embedding server answering every text
    /// with [`vector_of`], holding requests that name `marker` until the
    /// returned gate opens. Returns its base URL.
    fn serve(marker: &'static str) -> (String, Arc<Gate>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let gate = Arc::new(Gate {
            marker,
            open: Mutex::new(false),
            opened: Condvar::new(),
            held: AtomicUsize::new(0),
            texts: AtomicUsize::new(0),
        });
        let served = Arc::clone(&gate);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let gate = Arc::clone(&served);
                std::thread::spawn(move || {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let (header_end, length) = loop {
                        let read = stream.read(&mut chunk).unwrap_or(0);
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if let Some(end) =
                            buffer.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            let headers = String::from_utf8_lossy(&buffer[..end]).to_lowercase();
                            let length = headers
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length:"))
                                .and_then(|value| value.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            break (end + 4, length);
                        }
                    };
                    while buffer.len() < header_end + length {
                        let read = stream.read(&mut chunk).unwrap_or(0);
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                    }
                    let request: Value =
                        serde_json::from_slice(&buffer[header_end..header_end + length])
                            .unwrap_or(Value::Null);
                    let texts: Vec<String> = match &request["input"] {
                        Value::String(text) => vec![text.clone()],
                        Value::Array(items) => items
                            .iter()
                            .map(|item| item.as_str().unwrap_or_default().to_string())
                            .collect(),
                        _ => Vec::new(),
                    };
                    gate.texts.fetch_add(
                        texts
                            .iter()
                            .filter(|text| *text != "semantic index fingerprint probe")
                            .count(),
                        Ordering::SeqCst,
                    );
                    gate.wait_if_held(&texts);
                    let data: Vec<Value> = texts
                        .iter()
                        .enumerate()
                        .map(|(index, text)| json!({"index": index, "embedding": vector_of(text)}))
                        .collect();
                    let body = json!({"object": "list", "data": data}).to_string();
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes());
                });
            }
        });
        (format!("http://{address}/v1"), gate)
    }

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn wait_for<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(found) = check() {
                return found;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn search(ctx: &AppContext, id: &str, query: &str) -> Value {
        let request: RawRequest = serde_json::from_value(json!({
            "id": id,
            "command": "semantic_search",
            "query": query,
            "top_k": 5,
        }))
        .unwrap();
        serde_json::to_value(crate::commands::semantic_search::handle_semantic_search(
            &request, ctx,
        ))
        .unwrap()
    }

    /// While a fill is under way the plane serves what is filled and every
    /// surface says the answer is partial; it reports ready only once every
    /// file is filled, and the filled answer is cached under another
    /// generation than the partial one.
    #[test]
    fn a_fill_under_way_is_reported_partial_and_ready_only_once_every_file_is_filled() {
        crate::data_layer::runtime::force_on_for_test();
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        // More files than one fill round takes, the last of them held back
        // by the embedder, so the first round lands and the second waits.
        for index in 0..300 {
            write(
                &root,
                &format!("src/f{index:03}.rs"),
                &format!("/// Counts the widgets of batch {index}.\npub fn widget_count_{index}(items: &[u32]) -> usize {{\n    items.len() + {index}\n}}\n"),
            );
        }
        write(
            &root,
            "src/zz_last.rs",
            "/// The last file.\npub fn omega_marker() -> u32 {\n    7\n}\n",
        );
        let (base_url, gate) = serve("omega_marker");
        let config = crate::config::Config {
            project_root: Some(root.clone()),
            storage_dir: Some(storage.path().to_path_buf()),
            indexes: crate::config::IndexesConfig {
                trigram: true,
                semantic: true,
                callgraph: false,
            },
            semantic: SemanticBackendConfig {
                backend: SemanticBackend::OpenAiCompatible,
                model: "test-hashed".to_string(),
                base_url: Some(base_url),
                timeout_ms: 120_000,
                query_timeout_ms: 30_000,
                ..SemanticBackendConfig::default()
            },
            ..crate::config::Config::default()
        };
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        ctx.set_canonical_cache_root(root.clone());
        crate::semantic_admission::note_opened("data-layer-meaning-test", &root);
        let checkout = ctx.data_checkout().unwrap();
        wait_for("the text build", || checkout.view());

        // Asking starts the fill; its first round lands, the second waits.
        let partial = wait_for("the first fill round", || {
            DataSemantic::current(&ctx).and_then(|data| data.serving().cloned())
        });
        assert!(!partial.complete());
        assert_eq!(partial.coverage().files, 301);
        assert_eq!(
            partial.coverage().pending.len(),
            45,
            "{:?}",
            partial.coverage()
        );
        wait_for("the held request", || {
            (gate.held.load(Ordering::SeqCst) > 0).then_some(())
        });

        let status = ctx.build_status_snapshot();
        assert_eq!(status["semantic_index"]["status"], "building", "{status}");
        assert_eq!(status["semantic_index"]["files_pending"], 45, "{status}");
        // `aft warmup` keeps waiting while files wait for vectors.
        assert_eq!(
            crate::data_layer::surfaces::warm_meaning(&ctx),
            Some(crate::data_layer::surfaces::WarmState::Pending(
                "filling, files=256/301".to_string()
            ))
        );

        let reply = search(&ctx, "meaning-partial", "count the widgets of a batch");
        assert_eq!(reply["success"], true, "{reply}");
        assert_eq!(reply["status"], "partial", "{reply}");
        assert_eq!(reply["complete"], false, "{reply}");
        let text = reply["text"].as_str().unwrap();
        assert!(
            text.contains("semantic: 45 file(s) not yet embedded"),
            "{text}"
        );
        assert!(text.contains("semantic: partial"), "{text}");
        assert!(!reply["results"].as_array().unwrap().is_empty(), "{reply}");
        let partial_generation = reply["structuredContent"]["plan"]["snapshot_generation"].clone();

        gate.open();
        wait_for("the fill to finish", || {
            DataSemantic::current(&ctx)
                .and_then(|data| data.serving().cloned())
                .filter(|meaning| meaning.complete())
        });
        let status = ctx.build_status_snapshot();
        assert_eq!(status["semantic_index"]["status"], "ready", "{status}");
        assert_eq!(status["semantic_index"]["files_embedded"], 301, "{status}");
        assert_eq!(
            crate::data_layer::surfaces::warm_meaning(&ctx),
            Some(crate::data_layer::surfaces::WarmState::Ready)
        );

        let reply = search(&ctx, "meaning-ready", "count the widgets of a batch");
        assert_eq!(reply["success"], true, "{reply}");
        assert_eq!(reply["status"], "ready", "{reply}");
        assert_eq!(reply["complete"], true, "{reply}");
        let text = reply["text"].as_str().unwrap();
        assert!(!text.contains("not yet embedded"), "{text}");
        assert_ne!(
            reply["structuredContent"]["plan"]["snapshot_generation"], partial_generation,
            "a fill that lands changes the generation cached lists are keyed by"
        );
    }

    /// After an idle release drops the checkout, the next search binds it
    /// again from its stored publication and reports meaning search as not
    /// ready, with the lexical fallback, while the plane is read back (its
    /// model loaded and asked for its vector length); once it is back,
    /// searches are ready again and nothing was embedded again.
    #[test]
    fn a_search_after_an_idle_release_reports_the_plane_loading_until_it_is_back() {
        crate::data_layer::runtime::force_on_for_test();
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        write(
            &root,
            "src/config.rs",
            "/// Reads a configuration file from disk.\npub fn read_config(path: &str) -> String {\n    std::fs::read_to_string(path).unwrap_or_default()\n}\n",
        );
        let (base_url, gate) = serve("never-held");
        gate.open();
        let config = crate::config::Config {
            project_root: Some(root.clone()),
            storage_dir: Some(storage.path().to_path_buf()),
            indexes: crate::config::IndexesConfig {
                trigram: true,
                semantic: true,
                callgraph: false,
            },
            semantic: SemanticBackendConfig {
                backend: SemanticBackend::OpenAiCompatible,
                model: "test-hashed".to_string(),
                base_url: Some(base_url),
                timeout_ms: 120_000,
                query_timeout_ms: 30_000,
                ..SemanticBackendConfig::default()
            },
            ..crate::config::Config::default()
        };
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        ctx.set_canonical_cache_root(root.clone());
        crate::semantic_admission::note_opened("data-layer-meaning-release-test", &root);
        wait_for("the first fill", || {
            DataSemantic::current(&ctx)
                .and_then(|data| data.serving().cloned())
                .filter(|meaning| meaning.complete())
        });
        let embedded = gate.texts.load(Ordering::SeqCst);
        assert!(embedded > 0);

        let held = crate::data_layer::meaning::hold_model_loads_for_test(&root);
        assert!(
            ctx.try_release_idle_checkout_views(),
            "the root must be idle"
        );
        let reply = search(
            &ctx,
            "meaning-after-release",
            "read a configuration file from disk",
        );
        eprintln!("reply while the plane is read back: {}", reply["text"]);
        assert_eq!(reply["success"], true, "{reply}");
        assert_eq!(reply["semantic_status"], "building", "{reply}");
        assert_eq!(reply["complete"], false, "{reply}");
        assert_eq!(reply["lexical_only_fallback"], true, "{reply}");
        assert!(
            reply["note"].as_str().is_some_and(|note| !note.is_empty()),
            "the reply must disclose the lexical fallback: {reply}"
        );
        let text = reply["text"].as_str().unwrap_or_default();
        assert!(
            !text.lines().any(|line| line == "semantic: ready"),
            "the reply must not claim a ready semantic lane: {text}"
        );

        drop(held);
        wait_for("the plane to be back", || {
            DataSemantic::current(&ctx)
                .and_then(|data| data.serving().cloned())
                .filter(|meaning| meaning.complete())
        });
        assert_eq!(
            gate.texts.load(Ordering::SeqCst),
            embedded,
            "reading the plane back embedded files again"
        );
        let reply = search(
            &ctx,
            "meaning-after-reload",
            "read a configuration file from disk",
        );
        assert_eq!(reply["semantic_status"], "ready", "{reply}");
        assert_eq!(reply["complete"], true, "{reply}");
    }

    /// `aft index` fills a checkout's meaning plane in the foreground: every
    /// file gets its vectors before it returns, and a second fill of the same
    /// files embeds nothing, since the plane reuses what it stored.
    #[test]
    fn a_foreground_fill_embeds_every_file_and_a_second_one_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        // More files than one fill round takes, so the fill runs rounds
        // until none waits.
        for index in 0..300 {
            write(
                &root,
                &format!("src/f{index:03}.rs"),
                &format!("/// Counts the widgets of batch {index}.\npub fn widget_count_{index}(items: &[u32]) -> usize {{\n    items.len() + {index}\n}}\n"),
            );
        }
        let (base_url, gate) = serve("never-held");
        gate.open();
        let config = SemanticBackendConfig {
            backend: SemanticBackend::OpenAiCompatible,
            model: "test-hashed".to_string(),
            base_url: Some(base_url),
            timeout_ms: 120_000,
            query_timeout_ms: 30_000,
            ..SemanticBackendConfig::default()
        };

        let filled = crate::data_layer::surfaces::fill_root(&root, storage.path(), &config)
            .expect("the fill");
        assert_eq!(
            (filled.files, filled.files_embedded, filled.files_past_cap),
            (300, 300, 0),
            "{filled:?}"
        );
        assert!(filled.chunks >= 300, "{filled:?}");
        let embedded = gate.texts.load(Ordering::SeqCst);
        assert!(embedded >= 300, "{embedded} texts embedded");

        let again = crate::data_layer::surfaces::fill_root(&root, storage.path(), &config)
            .expect("the second fill");
        assert_eq!(again, filled);
        assert_eq!(
            gate.texts.load(Ordering::SeqCst),
            embedded,
            "the second fill embedded files again"
        );
    }

    /// A deterministic stand-in for an embedding model: each word of the
    /// text (identifiers split at case changes, digits and underscores,
    /// lowercased) adds a signed unit to one of 384 dimensions chosen by its
    /// digest. Texts sharing words point the same way, so a search ranks as
    /// a weak model would, and both sources get exactly the same vectors.
    fn hashed_embedding(text: &str) -> Vec<f32> {
        const LENGTH: usize = 384;
        let mut vector = vec![0.0f32; LENGTH];
        let mut word = String::new();
        let mut flush = |word: &mut String| {
            if word.is_empty() {
                return;
            }
            let digest = blake3::hash(word.as_bytes());
            let bytes = digest.as_bytes();
            let slot = usize::from(u16::from_le_bytes([bytes[0], bytes[1]])) % LENGTH;
            vector[slot] += if bytes[2] & 1 == 0 { 1.0 } else { -1.0 };
            word.clear();
        };
        let mut previous_lower = false;
        for character in text.chars() {
            if !character.is_alphanumeric() {
                flush(&mut word);
                previous_lower = false;
                continue;
            }
            if character.is_uppercase() && previous_lower {
                flush(&mut word);
            }
            previous_lower = character.is_lowercase() || character.is_ascii_digit();
            word.extend(character.to_lowercase());
        }
        flush(&mut word);
        vector
    }

    /// A result row's fields, with its scores' bits.
    fn row(result: &SemanticResult) -> String {
        format!(
            "{} {} {:?} {:?} {}-{} {} {:08x} {:08x} {} {} {:?}",
            result.file.display(),
            result.name,
            result.qualified_name,
            result.kind,
            result.start_line,
            result.end_line,
            result.exported,
            result.score.to_bits(),
            result.rank_score.to_bits(),
            result.cap_protected,
            result.source,
            result.snippet,
        )
    }

    /// One ranked row: its score's bits, its fields, and what it shows apart
    /// from its file (so two copies of one chunk in two files compare equal).
    type Ranked = (u32, String, String);

    fn ranked(result: &SemanticResult) -> Ranked {
        (
            result.score.to_bits(),
            row(result),
            format!(
                "{} {:?} {} {:?}",
                result.name, result.kind, result.exported, result.snippet
            ),
        )
    }

    /// How two answers to one question compare.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Verdict {
        Same,
        /// The same score sequence, and the same rows within every run of
        /// equal scores, the run cut by `k` excepted (its rows are any of
        /// the chunks with that score): only equal scores are in another
        /// order. `duplicates` when every reordered run holds copies of one
        /// chunk, `first_page` when a reordered row is among the first ten.
        TiesOnly {
            duplicates: bool,
            first_page: bool,
        },
        Different,
    }

    fn verdict(today: &[Ranked], data: &[Ranked], k: usize) -> Verdict {
        if today == data {
            return Verdict::Same;
        }
        if today.len() != data.len() || today.iter().zip(data).any(|(a, b)| a.0 != b.0) {
            return Verdict::Different;
        }
        let mut duplicates = true;
        let mut first_page = false;
        let mut start = 0;
        while start < today.len() {
            let end = start + today[start..].partition_point(|row| row.0 == today[start].0);
            let (left, right) = (&today[start..end], &data[start..end]);
            if left != right {
                let cut = end == today.len() && today.len() == k;
                let mut left_rows: Vec<&String> = left.iter().map(|row| &row.1).collect();
                let mut right_rows: Vec<&String> = right.iter().map(|row| &row.1).collect();
                left_rows.sort_unstable();
                right_rows.sort_unstable();
                if !cut && left_rows != right_rows {
                    return Verdict::Different;
                }
                duplicates &= left.iter().chain(right).all(|row| row.2 == left[0].2);
                first_page |= left
                    .iter()
                    .zip(right)
                    .position(|(a, b)| a != b)
                    .is_some_and(|offset| start + offset < 10);
            }
            start = end;
        }
        Verdict::TiesOnly {
            duplicates,
            first_page,
        }
    }

    /// What one comparison found over the query list, recorded as it goes
    /// so no answer is kept past its comparison.
    #[derive(Default)]
    struct Tally {
        queries: usize,
        differing: usize,
        ties_only: usize,
        ties_only_duplicates: usize,
        ties_only_first_page: usize,
        /// Queries whose first ten rows today hold two equal scores.
        ties_in_first_ten: usize,
        today: Duration,
        data: Duration,
        examples: Vec<Value>,
        tie_examples: Vec<Value>,
    }

    impl Tally {
        fn record(
            &mut self,
            query: &str,
            k: usize,
            today: (Vec<Ranked>, Duration),
            data: (Vec<Ranked>, Duration),
        ) {
            self.queries += 1;
            self.today += today.1;
            self.data += data.1;
            let first_ten = &today.0[..today.0.len().min(10)];
            self.ties_in_first_ten +=
                usize::from(first_ten.windows(2).any(|pair| pair[0].0 == pair[1].0));
            let examples = match verdict(&today.0, &data.0, k) {
                Verdict::Same => return,
                Verdict::TiesOnly {
                    duplicates,
                    first_page,
                } => {
                    self.ties_only += 1;
                    self.ties_only_duplicates += usize::from(duplicates);
                    self.ties_only_first_page += usize::from(first_page);
                    &mut self.tie_examples
                }
                Verdict::Different => {
                    self.differing += 1;
                    &mut self.examples
                }
            };
            if examples.len() >= 25 {
                return;
            }
            let parted = today.0.iter().zip(&data.0).position(|(a, b)| a != b);
            let clip = |line: Option<&Ranked>| {
                line.map(|line| line.1.chars().take(400).collect::<String>())
            };
            examples.push(json!({
                "query": query,
                "today_rows": today.0.len(),
                "data_rows": data.0.len(),
                "first_parting_row": parted,
                "today_at_parting": parted.and_then(|index| clip(today.0.get(index))),
                "data_at_parting": parted.and_then(|index| clip(data.0.get(index))),
            }));
        }

        fn summary(&self) -> Value {
            let per_call =
                |total: Duration| total.as_secs_f64() * 1000.0 / self.queries.max(1) as f64;
            json!({
                "queries": self.queries,
                "differing": self.differing,
                "allowed_ties_in_path_order": self.ties_only,
                "ties_between_copies_of_one_chunk": self.ties_only_duplicates,
                "ties_reordered_in_first_ten": self.ties_only_first_page,
                "ties_in_first_ten": self.ties_in_first_ten,
                "today_ms_per_call": per_call(self.today),
                "data_ms_per_call": per_call(self.data),
                "examples": self.examples,
                "tie_examples": self.tie_examples,
            })
        }
    }

    fn timed<T>(work: impl FnOnce() -> T) -> (T, Duration) {
        let started = Instant::now();
        let value = work();
        (value, started.elapsed())
    }

    /// Asks today's semantic index and the data layer's meaning plane, built
    /// from the same checkout with the same embedder, the semantic lane's
    /// question for each distinct query of a real query list, and writes
    /// what differs and what each costs. Answers that differ only in the
    /// order of equal scores are counted apart (see [`Verdict::TiesOnly`]).
    /// Run on aft's own checkout, it shows that the semantic lane answers
    /// as today's index does before aft_search reads the plane instead:
    ///
    /// `AFT_MEANING_PARITY_ROOT=<checkout> AFT_MEANING_PARITY_QUERIES=<one
    /// query per line> AFT_MEANING_PARITY_OUT=<report.json> cargo test -p
    /// agent-file-tools --lib -- --ignored meaning_lane_matches`
    ///
    /// Today's index is built from the files configure's semantic walk
    /// finds, as configure builds it; the plane from the data layer's scan
    /// of the same tree. `AFT_MEANING_PARITY_THREADS` spreads the queries
    /// over threads (per-call costs then include the contention);
    /// `AFT_MEANING_PARITY_STRIDE=n` asks every nth query only.
    /// `AFT_MEANING_PARITY_EMBEDDER=local` embeds with the local model AFT
    /// ships by default instead of [`hashed_embedding`], each distinct text
    /// once for both sources; it needs the model's ONNX Runtime
    /// (`ORT_DYLIB_PATH`) and its files or the network to fetch them
    /// (`FASTEMBED_CACHE_DIR`).
    #[test]
    #[ignore = "reads the checkout and query list named by AFT_MEANING_PARITY_*"]
    fn meaning_lane_matches_todays_index_on_real_queries() {
        use crate::data_layer::checkout::{Checkout, DataLayer};
        use crate::data_layer::vectors::{FillBudget, Producer, VectorPlane};
        use crate::semantic_index::SemanticIndexFingerprint;

        let variable = |name: &str| std::env::var(name).ok();
        let root = std::fs::canonicalize(variable("AFT_MEANING_PARITY_ROOT").unwrap()).unwrap();
        let queries =
            std::fs::read_to_string(variable("AFT_MEANING_PARITY_QUERIES").unwrap()).unwrap();
        let out = std::path::PathBuf::from(variable("AFT_MEANING_PARITY_OUT").unwrap());
        let threads = variable("AFT_MEANING_PARITY_THREADS")
            .and_then(|threads| threads.parse::<usize>().ok())
            .unwrap_or(1)
            .max(1);
        let stride = variable("AFT_MEANING_PARITY_STRIDE")
            .and_then(|stride| stride.parse::<usize>().ok())
            .unwrap_or(1)
            .max(1);
        let config = SemanticBackendConfig::default();
        let mut local = (variable("AFT_MEANING_PARITY_EMBEDDER").as_deref() == Some("local"))
            .then(|| crate::semantic_index::SemanticEmbeddingModel::from_config(&config).unwrap());
        let fingerprint = match local.as_mut() {
            Some(model) => model.fingerprint(&config).unwrap(),
            None => SemanticIndexFingerprint::for_config_dimension(&config, 384),
        };
        let caps = fingerprint.embed_text_caps;
        // Both sources embed the same texts; each distinct one is embedded
        // once and its vector reused, so they score identical vectors.
        let mut vectors: std::collections::HashMap<String, Vec<f32>> =
            std::collections::HashMap::new();
        let mut embed = |texts: Vec<String>| -> Result<Vec<Vec<f32>>, String> {
            let missing: Vec<String> = texts
                .iter()
                .filter(|text| !vectors.contains_key(*text))
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            if !missing.is_empty() {
                let embedded = match local.as_mut() {
                    Some(model) => model.embed(missing.clone())?,
                    None => missing.iter().map(|text| hashed_embedding(text)).collect(),
                };
                vectors.extend(missing.into_iter().zip(embedded));
            }
            Ok(texts.iter().map(|text| vectors[text].clone()).collect())
        };

        // Today's index, from configure's semantic walk.
        let (today, today_ms) = timed(|| {
            let files = crate::commands::configure::walk_semantic_project_files_bounded(
                &root,
                config.max_files,
            )
            .unwrap();
            let index =
                SemanticIndex::build_with_caps(&root, &files, &mut embed, 64, caps).unwrap();
            (files, index)
        });
        let (today_files, today) = today;

        // The data layer's plane over a fresh store.
        let store = tempfile::tempdir().unwrap();
        let family = aft_store::ids::FamilyId::from_digest(aft_store::ids::Digest128::of(
            b"meaning lane parity",
        ));
        let ((checkout, plane, fill), data_ms) = timed(|| {
            let checkout = Arc::new(
                Checkout::bind(
                    Arc::new(DataLayer::new(256 << 20, threads.max(2))),
                    store.path(),
                    family,
                    &root,
                )
                .unwrap(),
            );
            checkout.build().unwrap();
            let plane = VectorPlane::new(
                Arc::clone(&checkout),
                Producer::from_fingerprint(&fingerprint),
                config.max_files,
            );
            let fill = plane
                .fill(
                    &mut embed,
                    FillBudget {
                        max_batch: 64,
                        files_per_round: 256,
                    },
                    &mut |_| {},
                )
                .unwrap();
            (checkout, plane, fill)
        });
        let view = checkout.view().unwrap();
        plane.refresh().unwrap();
        let meaning = DataMeaning {
            snapshot: plane.snapshot(&view).unwrap(),
            failure: None,
        };
        let coverage = meaning.coverage().clone();
        let today_set: std::collections::BTreeSet<_> = today_files.iter().cloned().collect();
        let data_set: std::collections::BTreeSet<_> = view
            .current_state()
            .paths()
            .iter()
            .filter(|record| {
                record.kind != aft_store::checkout::PathKind::Symlink
                    && crate::semantic_index::is_semantic_indexed_extension(Path::new(
                        &String::from_utf8_lossy(&record.path).to_string(),
                    ))
            })
            .map(|record| root.join(String::from_utf8_lossy(&record.path).to_string()))
            .collect();
        eprintln!(
            "today: {} files, {} chunks in {:.0} ms; data layer: {} files, {} chunks embedded in {:.0} ms",
            today_files.len(),
            today.len(),
            today_ms.as_secs_f64() * 1000.0,
            coverage.files,
            coverage.chunks_embedded,
            data_ms.as_secs_f64() * 1000.0,
        );

        let mut seen = std::collections::HashSet::new();
        let queries = queries
            .lines()
            .map(str::trim)
            .filter(|query| !query.is_empty() && seen.insert(query.to_string()))
            .step_by(stride)
            .collect::<Vec<_>>();
        let query_vectors = queries
            .chunks(64)
            .flat_map(|batch| embed(batch.iter().map(|query| query.to_string()).collect()).unwrap())
            .collect::<Vec<_>>();
        let lane_k = crate::commands::semantic_search::SEMANTIC_ENUMERATION_LIMIT + 1;
        let next = AtomicUsize::new(0);
        let tallies = Mutex::new(std::collections::BTreeMap::<&'static str, Tally>::new());
        let started = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    let today_index = MeaningIndex::Snapshot(&today);
                    let data_index = MeaningIndex::Data(&meaning);
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(query) = queries.get(index) else {
                            break;
                        };
                        let vector = &query_vectors[index];
                        let mut found = Vec::new();
                        for (name, include_tests, k) in [
                            ("lane", false, lane_k),
                            ("lane_include_tests", true, lane_k),
                            ("recall_audit_full_ranking", false, usize::MAX),
                        ] {
                            let include = |file: &Path| {
                                crate::commands::semantic_search::path_allowed_by_include_tests(
                                    file,
                                    &root,
                                    include_tests,
                                )
                            };
                            let ask = |source: MeaningIndex<'_>| {
                                timed(|| {
                                    source
                                        .search_filtered(vector, k, &include)
                                        .unwrap()
                                        .iter()
                                        .map(ranked)
                                        .collect::<Vec<_>>()
                                })
                            };
                            found.push((name, k, ask(today_index), ask(data_index)));
                        }
                        let mut tallies = tallies.lock().unwrap();
                        for (name, k, today, data) in found {
                            tallies
                                .entry(name)
                                .or_default()
                                .record(query, k, today, data);
                        }
                        let finished = tallies.get("lane").map_or(0, |tally| tally.queries);
                        if finished % 500 == 0 {
                            eprintln!(
                                "{finished} queries in {:.0} s",
                                started.elapsed().as_secs_f64()
                            );
                        }
                    }
                });
            }
        });
        let tallies = tallies.into_inner().unwrap();
        let only = |left: &std::collections::BTreeSet<std::path::PathBuf>,
                    right: &std::collections::BTreeSet<std::path::PathBuf>| {
            left.difference(right)
                .take(25)
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
        };
        let mut report = json!({
            "root": root,
            "embedder": if local.is_some() { "local" } else { "hashed" },
            "stride": stride,
            "distinct_queries": queries.len(),
            "threads": threads,
            "today_files": today_files.len(),
            "today_chunks": today.len(),
            "today_build_ms": today_ms.as_secs_f64() * 1000.0,
            "data_build_and_fill_ms": data_ms.as_secs_f64() * 1000.0,
            "data_coverage": coverage,
            "data_fill": fill,
            "files_only_today": only(&today_set, &data_set),
            "files_only_today_count": today_set.difference(&data_set).count(),
            "files_only_data": only(&data_set, &today_set),
            "files_only_data_count": data_set.difference(&today_set).count(),
        });
        for (name, tally) in &tallies {
            report[*name] = tally.summary();
            println!(
                "{name}: {} of {} differ, {} only in the order of equal scores ({} of them copies of one chunk, {} within the first ten); today {:.3} ms, data layer {:.3} ms per call",
                tally.differing,
                tally.queries,
                tally.ties_only,
                tally.ties_only_duplicates,
                tally.ties_only_first_page,
                tally.today.as_secs_f64() * 1000.0 / tally.queries.max(1) as f64,
                tally.data.as_secs_f64() * 1000.0 / tally.queries.max(1) as f64,
            );
        }
        std::fs::write(&out, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        let failing = tallies
            .iter()
            .filter(|(_, tally)| tally.differing > 0)
            .map(|(name, tally)| format!("{name}: {} of {}", tally.differing, tally.queries))
            .collect::<Vec<_>>();
        // Equal scores are in path order on the data layer and in today's
        // build order (newest file first) there; every other difference
        // fails.
        assert!(
            failing.is_empty(),
            "semantic lane answers differ: {failing:?}"
        );
    }
}
