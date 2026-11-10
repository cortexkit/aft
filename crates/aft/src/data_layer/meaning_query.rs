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

use std::path::PathBuf;
use std::sync::Arc;

use crate::context::AppContext;
use crate::data_layer::meaning::{self, FillFailure, MeaningState};
use crate::data_layer::status::SemanticIndexStatus;
use crate::data_layer::vectors::{Coverage, MeaningSnapshot};
use crate::semantic_index::SemanticSkipReason;

/// The stage a meaning plane reports while its checkout has published
/// nothing yet, so there is no view to fill.
pub(crate) const DATA_LAYER_BUILD_STAGE: &str = "data_layer_build";

/// A data-layer meaning plane pinned at one generation of the family's
/// vectors for one view, with what the last fill reported.
#[derive(Debug)]
pub(crate) struct DataMeaning {
    pub(crate) snapshot: MeaningSnapshot,
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
    /// As [`Self::current`] for status reads: never binds a checkout and
    /// never starts a fill.
    pub(crate) fn probe(ctx: &AppContext) -> Option<Self> {
        use crate::data_layer::surfaces::DataCheckoutProbe;
        if !ctx.config().indexes.semantic {
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

    pub(crate) fn from_state(state: MeaningState) -> Option<Self> {
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

#[cfg(test)]
pub(crate) mod tests {
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
    pub(crate) struct Gate {
        marker: &'static str,
        open: Mutex<bool>,
        opened: Condvar,
        held: AtomicUsize,
        /// Texts embedded, the fingerprint probe aside.
        pub(crate) texts: AtomicUsize,
    }

    impl Gate {
        pub(crate) fn open(&self) {
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
    pub(crate) fn serve(marker: &'static str) -> (String, Arc<Gate>) {
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
}
