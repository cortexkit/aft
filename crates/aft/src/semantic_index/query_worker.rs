use super::{QueryEmbeddingCache, SharedLocalEmbeddingProvider};
use crate::slog_warn;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

struct LocalQueryEmbedRequest {
    texts: Vec<String>,
    cache_key: String,
    response: crossbeam_channel::Sender<Result<Vec<Vec<f32>>, String>>,
}

pub(super) struct LocalQueryEmbedWorker {
    requests: crossbeam_channel::Sender<LocalQueryEmbedRequest>,
    busy: Arc<AtomicBool>,
    /// Queries wait their turn until the earlier caller gets a reply or times out.
    pub(super) submissions: Mutex<()>,
}

impl LocalQueryEmbedWorker {
    pub(super) fn start(
        model: SharedLocalEmbeddingProvider,
        query_cache: Arc<Mutex<QueryEmbeddingCache>>,
    ) -> Result<Self, String> {
        let (requests, receiver) = crossbeam_channel::unbounded::<LocalQueryEmbedRequest>();
        let busy = Arc::new(AtomicBool::new(false));
        let worker_busy = Arc::clone(&busy);
        std::thread::Builder::new()
            .name("aft-local-query-embed".to_string())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    let result = {
                        let mut provider = model.lock().unwrap_or_else(PoisonError::into_inner);
                        catch_unwind(AssertUnwindSafe(|| provider.embed(&request.texts)))
                    };
                    let Ok(result) = result else {
                        slog_warn!(
                            "local query embedding provider panicked; resuming query worker"
                        );
                        // Drop only this reply, preserving the disconnected-request error.
                        worker_busy.store(false, Ordering::Release);
                        continue;
                    };
                    let result = result.map_err(|error| format!("failed to embed batch: {error}"));
                    if let Ok(vectors) = &result {
                        if let Some(vector) = vectors.first() {
                            query_cache
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .insert(request.cache_key, vector.clone());
                        }
                    }
                    worker_busy.store(false, Ordering::Release);
                    let _ = request.response.send(result);
                }
            })
            .map_err(|error| format!("failed to start local query embed worker: {error}"))?;
        Ok(Self {
            requests,
            busy,
            submissions: Mutex::new(()),
        })
    }

    pub(super) fn try_submit(
        &self,
        texts: Vec<String>,
        cache_key: String,
    ) -> Option<crossbeam_channel::Receiver<Result<Vec<Vec<f32>>, String>>> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        let (response, receiver) = crossbeam_channel::bounded(1);
        if self
            .requests
            .send(LocalQueryEmbedRequest {
                texts,
                cache_key,
                response,
            })
            .is_err()
        {
            self.busy.store(false, Ordering::Release);
            return None;
        }
        Some(receiver)
    }
}
