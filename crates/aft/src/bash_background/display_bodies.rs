use super::*;

impl BgTaskRegistry {
    pub(super) fn display_bodies(
        &self,
        task: &Arc<BgTask>,
        metadata: &PersistedTask,
        buffer: &BgBuffer,
    ) -> crate::bash_background::frames::BashBodies {
        use crate::bash_background::buffer::StreamKind;
        use crate::bash_background::frames::{bash_bodies, bash_tail};
        let bytes = metadata
            .remote
            .as_ref()
            .filter(|_| !metadata.local_fallback_started)
            .map(|remote| {
                remote
                    .stdout_len
                    .saturating_add(remote.stderr_len)
                    .saturating_add(remote.unknown_len)
            })
            .unwrap_or_else(|| {
                let stdout = buffer.stream_len(StreamKind::Stdout);
                if metadata.mode == BgMode::Pty {
                    stdout
                } else {
                    stdout.saturating_add(buffer.stream_len(StreamKind::Stderr))
                }
            });
        let secrets = task.completion_secrets.lock().ok();
        let masker = secrets.as_deref().and_then(Option::as_ref);
        let safe = masker.is_some() || metadata.has_safe_completion_redaction_after_restart();
        let mask = |text: &str| -> Option<String> {
            let masked = masker.map_or_else(
                || crate::log_redact::aft_redactor(text),
                |masker| masker.redact(text),
            );
            crate::log_redact::redact_ticket_digest(
                &masked,
                metadata.completion_ticket_digest.as_deref(),
            )
            .ok()
            .map(|text| text.into_owned())
        };
        let streams = safe
            .then(|| buffer.read_complete_for_notice())
            .flatten()
            .and_then(|(stdout, stderr)| Some((mask(&stdout)?, mask(&stderr)?)));
        let tail = streams.as_ref().and_then(|(stdout, stderr)| {
            bash_tail(metadata, stdout, stderr, &mask)
        });
        let mut bodies = bash_bodies(metadata, Some(bytes), streams);
        bodies.tail = tail;
        if let Some(refusal) = (!metadata.local_fallback_started)
            .then(|| remote::remote_refusal(metadata))
            .flatten()
        {
            bodies.facts["reason"] = serde_json::json!(refusal.code);
            bodies.facts.as_object_mut().unwrap().remove("endReason");
            bodies.facts.as_object_mut().unwrap().remove("exitCode");
        }
        bodies
    }

    pub(super) fn capture_display_terminal(
        &self,
        task: &Arc<BgTask>,
        metadata: &PersistedTask,
        buffer: &BgBuffer,
    ) {
        if metadata
            .display
            .as_ref()
            .is_none_or(|display| display.attempted)
        {
            return;
        }
        let bodies = self.display_bodies(task, metadata, buffer);
        // The handler can release its reply while the terminal snapshot is
        // being built. Claim from live task metadata, not that older snapshot.
        let display = {
            let mut db = DeferredDbWrites::new(self, task);
            let Ok(mut state) = task.lock_state() else {
                return;
            };
            let Some(display) = state
                .metadata
                .display
                .as_ref()
                .filter(|display| !display.attempted)
                .cloned()
            else {
                return;
            };
            if display.reply_produced && display.deferred {
                state.metadata.display.as_mut().unwrap().attempted = true;
                if self
                    .persist_task_locked(task, &state.metadata, &mut db)
                    .is_err()
                {
                    return;
                }
            }
            display
        };
        crate::bash_background::frames::bash_terminal(
            &display,
            bodies,
            &metadata.task_id,
            metadata.duration_ms,
        );
    }
}
