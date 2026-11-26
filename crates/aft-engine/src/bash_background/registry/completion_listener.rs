use super::*;

pub(super) type Listener = Arc<dyn Fn(&str) + Send + Sync>;

impl BgTaskRegistry {
    #[doc(hidden)]
    pub fn set_completion_delivery_listener(&self, listener: Listener) {
        *self.inner.completion_delivery_listener.lock().unwrap() = Some(listener);
    }

    pub(super) fn notify_completion_delivered(&self, task: &str) {
        let listener = self
            .inner
            .completion_delivery_listener
            .lock()
            .unwrap()
            .clone();
        if let Some(listener) = listener {
            listener(task);
        }
    }

    pub fn pending_completions_for_session(&self, session_id: &str) -> Vec<BgCompletion> {
        self.inner
            .completions
            .lock()
            .map(|completions| {
                completions
                    .iter()
                    .filter(|completion| completion_matches_session(completion, Some(session_id)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    #[doc(hidden)]
    pub fn pending_completion_ids(&self) -> HashMap<String, HashSet<String>> {
        let mut ids: HashMap<String, HashSet<String>> = HashMap::new();
        if let Ok(completions) = self.inner.completions.lock() {
            for completion in completions.iter() {
                ids.entry(completion.session_id.clone())
                    .or_default()
                    .insert(completion.task_id.clone());
            }
        }
        ids
    }

    #[doc(hidden)]
    pub fn pending_ingress_completions(&self, budget: usize) -> Option<Vec<BgCompletion>> {
        let queue = self.inner.completions.try_lock().ok()?;
        Some(
            queue
                .iter()
                .filter(|completion| completion.ingress_missed.load(Ordering::Acquire))
                .take(budget)
                .cloned()
                .collect(),
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BgCompletion {
    // Shared with the live push so saturation marks the existing registry row,
    // rather than collecting dropped frame copies in a second queue.
    #[serde(skip_serializing)]
    pub ingress_missed: Arc<AtomicBool>,
    #[serde(
        rename = "_bash_changed_files",
        skip_serializing_if = "Option::is_none"
    )]
    pub changed_files: Option<crate::protocol::Report>,
    pub task_id: String,
    /// Intentionally omitted from serialized completion payloads: push frames
    /// carry `session_id` at the BashCompletedFrame envelope level for routing.
    #[serde(skip_serializing)]
    pub session_id: String,
    #[serde(skip_serializing)]
    pub launching_identity: Option<crate::bash_background::TaskIdentity>,
    pub status: BgTaskStatus,
    pub exit_code: Option<i64>,
    pub command: String,
    /// Small head+tail preview of the cached terminal render at completion time,
    /// cached so push-frame consumers and `bash_drain_completions` callers see
    /// the same preview without racing against later output rotation. Empty
    /// when not captured (e.g., persisted task seen on startup before buffer
    /// reattachment).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_preview: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash_output_list_envelope: Option<ListEnvelope>,
    /// True when the captured tail is shorter than the actual output (because
    /// rotation occurred or the output exceeds the preview cap). Plugins use
    /// this to render a `…` prefix and signal that `bash_status` would return
    /// more.
    #[serde(default, skip_serializing_if = "is_false")]
    pub output_truncated: bool,
    /// Token count for the capped raw stdout+stderr payload before compression.
    /// Omitted when capture or tokenization fails, or the exact count will not fit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_tokens: Option<u32>,
    /// Token count for the compressed output generated from the same capped
    /// raw payload. Omitted when either token count is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compressed_tokens: Option<u32>,
    /// True when exact token counts are unavailable, including capture/read
    /// failures, pre-tokenizer errors and counts that cannot be represented.
    #[serde(default, skip_serializing_if = "is_false")]
    pub tokens_skipped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    /// The build server's memory-kill cause and measurements, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_oom_kill: Option<remote::RemoteOomKill>,
    /// Output capture was incomplete, without changing the command's outcome.
    #[serde(default, skip_serializing_if = "is_false")]
    pub output_incomplete: bool,
    pub live_descendants: Option<Vec<LiveDescendant>>,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub live_descendants_omitted: usize,
    /// For a finished task, why `live_descendants` is null, or, with
    /// `listing_incomplete`, why the list may be only part of the group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_descendants_unavailable: Option<LiveDescendantsUnavailable>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_descendants_summary: Option<String>,
}

impl BgCompletion {
    #[doc(hidden)]
    pub fn into_push_frame(self) -> PushFrame {
        let mut frame = BashCompletedFrame::new(
            self.task_id,
            self.session_id,
            self.status,
            self.exit_code,
            self.command,
            self.output_preview,
            self.output_truncated,
            self.original_tokens,
            self.compressed_tokens,
            self.tokens_skipped,
        );
        frame.bash_output_list_envelope = self.bash_output_list_envelope;
        frame.ingress_missed = Some(self.ingress_missed);
        frame.launching_identity = self.launching_identity;
        frame.changed_files = self.changed_files;
        frame.status_reason = self.status_reason;
        frame.remote_oom_kill = self.remote_oom_kill;
        frame.output_incomplete = self.output_incomplete;
        frame.live_descendants = self.live_descendants;
        frame.live_descendants_omitted = self.live_descendants_omitted;
        frame.live_descendants_unavailable = self.live_descendants_unavailable;
        frame.live_descendants_summary = self.live_descendants_summary;
        PushFrame::BashCompleted(frame)
    }
}
