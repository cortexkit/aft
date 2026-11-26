use super::*;

impl BgTaskRegistry {
    /// Records the display identity of the task that an original `bash` reply
    /// started. A later `bash_status` or `bash_watch` does not own that display.
    pub(crate) fn capture_display_reply(&self, response: &serde_json::Value) {
        use crate::bash_background::frames;

        if !frames::enabled() {
            return;
        }
        let Some(task_id) = response.get("task_id").and_then(serde_json::Value::as_str) else {
            return;
        };
        let Some(task) = self.task(task_id) else {
            return;
        };
        let (mut metadata, buffer) = {
            let Ok(state) = task.lock_state() else {
                return;
            };
            if state
                .metadata
                .display
                .as_ref()
                .is_none_or(|display| display.reply_produced)
            {
                return;
            }
            (state.metadata.clone(), state.buffer.clone())
        };
        let bodies = self.display_bodies(&task, &metadata, &buffer);
        let deferred = !Self::response_status_is_terminal(
            response.get("status").and_then(serde_json::Value::as_str),
        );
        let Some(display) = metadata.display.as_mut() else {
            return;
        };
        frames::bash_reply(display, bodies, deferred);
        let (handoff, terminal) = {
            let mut db = DeferredDbWrites::new(self, &task);
            let Ok(mut state) = task.lock_state() else {
                return;
            };
            if let Some(current) = state.metadata.display.as_mut() {
                if !current.reply_produced {
                    // A terminal worker may already have claimed completion;
                    // copying reply facts must never reset its durable marker.
                    current.reply_produced = display.reply_produced;
                    current.deferred = display.deferred;
                    current.held_ms = display.held_ms;
                }
            }
            let persisted = self
                .persist_task_locked(&task, &state.metadata, &mut db)
                .is_ok();
            let terminal = state
                .metadata
                .is_terminal()
                .then(|| (state.metadata.clone(), state.buffer.clone()));
            let handoff = persisted.then(|| state.metadata.display.clone()).flatten();
            (handoff, terminal)
        };
        if let Some((metadata, buffer)) = terminal {
            self.capture_display_terminal(&task, &metadata, &buffer);
        }
        if let Some(display) = handoff.filter(|display| display.reply_produced && display.deferred)
        {
            frames::bash_reply_persisted(&display);
        }
    }

    pub(super) fn display_bodies(
        &self,
        task: &Arc<BgTask>,
        metadata: &PersistedTask,
        buffer: &BgBuffer,
    ) -> crate::bash_background::frames::BashBodies {
        use crate::bash_background::buffer::StreamKind;
        use crate::bash_background::frames::bash_bodies;
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
        let (streams, unread, tail) =
            completion_preview::capture_reads::masked_display_streams(task, metadata, buffer)
                .map_or((None, 0, None), |read| {
                    (Some(read.streams), read.unread, read.tail)
                });
        let mut bodies = bash_bodies(metadata, Some(bytes), streams);
        bodies.tail = tail;
        if unread > 0 {
            // `cut_detail` adds the bytes it cuts to these.
            bodies.detail["truncated"] = serde_json::json!(true);
            bodies.detail["omittedBytes"] = serde_json::json!(unread);
        }
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
