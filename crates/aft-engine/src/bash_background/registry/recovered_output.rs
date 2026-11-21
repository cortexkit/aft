use super::*;

impl BgTaskRegistry {
    pub(super) fn unknown_bash_result_from_render(
        &self,
        metadata: &PersistedTask,
        paths: &TaskPaths,
        render: Option<&TerminalOutputCache>,
    ) -> crate::db::call_ledger::RecordedFrame {
        let preview = render
            .map(|cache| {
                self.terminal_preview_for_reply(metadata, cache, Some(paths))
                    .0
            })
            .unwrap_or_default();
        crate::db::call_ledger::unknown_bash_result(
            &metadata.task_id,
            remote_job_id(metadata).as_deref(),
            &preview,
        )
    }
}
