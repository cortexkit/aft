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
}
