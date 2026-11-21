//! Task metadata updates that preserve terminal execution facts.

use super::*;

impl PersistedTask {
    #[doc(hidden)]
    pub fn lifecycle_scope(&self) -> Option<&subc_protocol::scope::ScopeStamp> {
        self.lifecycle_scope.as_ref().or(self.scope.as_ref())
    }
}

pub fn update_task_at<F>(task: &ResolvedTask, update: F) -> io::Result<PersistedTask>
where
    F: FnOnce(&mut PersistedTask),
{
    let mut metadata = read_task_at(task)?;
    let original_terminal = metadata.is_terminal();
    let original = metadata.clone();
    update(&mut metadata);
    metadata.schema_version = SCHEMA_VERSION;
    if original_terminal {
        // Terminal execution facts are immutable; delivery bookkeeping can
        // still advance as a consumer receives or acknowledges the result.
        // Live descendants may also follow a confirmed module rename without
        // changing the terminal command's original spawn identity.
        let lifecycle_scope = metadata.lifecycle_scope.clone();
        let completion_delivered = metadata.completion_delivered;
        let runner_completion_offers = metadata
            .runner_completion_offers
            .max(original.runner_completion_offers);
        let runner_completion_delivery = metadata.runner_completion_delivery.clone();
        let runner_completion_notice = original
            .runner_completion_notice
            .clone()
            .or(metadata.runner_completion_notice);
        let runner_notice_policy = original
            .runner_notice_policy
            .or(metadata.runner_notice_policy);
        let runner_reminder_elapsed_ms = original
            .runner_reminder_elapsed_ms
            .max(metadata.runner_reminder_elapsed_ms);
        metadata = original;
        metadata.lifecycle_scope = lifecycle_scope;
        metadata.completion_delivered = completion_delivered;
        metadata.runner_completion_offers = runner_completion_offers;
        metadata.runner_completion_notice = runner_completion_notice;
        metadata.runner_completion_delivery = runner_completion_delivery;
        metadata.runner_notice_policy = runner_notice_policy;
        metadata.runner_reminder_elapsed_ms = runner_reminder_elapsed_ms;
        metadata.schema_version = SCHEMA_VERSION;
    }
    write_task_at(task, &metadata)?;
    Ok(metadata)
}
