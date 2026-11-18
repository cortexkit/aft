use super::super::process::{recorded_process_liveness, ProcessLiveness};
#[cfg(target_os = "linux")]
use super::super::{
    memory_limit::MemoryPolicy,
    task_scope::{lookup_local, lookup_state, LocalScopeLookup, ScopeLookup},
};
#[cfg(target_os = "linux")]
use super::BgMode;
use super::{BgTaskRegistry, PersistedTask};
#[cfg(target_os = "linux")]
use std::fs;

impl BgTaskRegistry {
    /// Withdrawal needs proof of death before freezing a missing-registry task's answer.
    pub(crate) fn unregistered_task_process_liveness(
        &self,
        metadata: &PersistedTask,
    ) -> ProcessLiveness {
        if metadata.remote.is_some() {
            return ProcessLiveness::Unknown;
        }
        let pids = Self::persisted_task_recorded_pids(metadata);
        if pids.is_empty() {
            return ProcessLiveness::Unknown;
        }
        let mut gone = true;
        for pid in pids {
            match recorded_process_liveness(pid, metadata.started_at) {
                ProcessLiveness::Gone => {}
                ProcessLiveness::Unknown => return ProcessLiveness::Unknown,
                ProcessLiveness::Alive => gone = false,
            }
        }
        #[cfg(target_os = "linux")]
        if gone
            && (metadata.task_scope_runtime_ms.is_some()
                || metadata
                    .memory_policy
                    .as_ref()
                    .is_some_and(|policy| policy.kind == MemoryPolicy::TASK_SCOPE_MEMORY_KIND))
        {
            return Self::unregistered_task_scope_liveness(metadata);
        }
        #[cfg(target_os = "linux")]
        if !gone && metadata.mode == BgMode::Pipes {
            // Validate the recorded child identity and scope membership. A failed
            // adoption says nothing about whether the task has stopped.
            if self.recover_persisted_task_scope(metadata).is_err() {
                return ProcessLiveness::Unknown;
            }
        }
        if gone {
            ProcessLiveness::Gone
        } else {
            ProcessLiveness::Alive
        }
    }

    #[cfg(target_os = "linux")]
    fn unregistered_task_scope_liveness(metadata: &PersistedTask) -> ProcessLiveness {
        let recorded = metadata
            .memory_policy
            .as_ref()
            .filter(|policy| policy.kind == MemoryPolicy::TASK_SCOPE_MEMORY_KIND)
            .and_then(|policy| policy.cgroup.as_deref());
        let scope = match lookup_local(&metadata.task_id, recorded) {
            Ok(LocalScopeLookup::Absent) => return ProcessLiveness::Gone,
            Ok(LocalScopeLookup::Present(scope)) => scope,
            Ok(LocalScopeLookup::Unknown) => match lookup_state(&metadata.task_id) {
                Ok(ScopeLookup::Absent) => return ProcessLiveness::Gone,
                Ok(ScopeLookup::Present(scope)) => scope,
                Ok(ScopeLookup::Unreachable) | Err(_) => return ProcessLiveness::Unknown,
            },
            Err(_) => return ProcessLiveness::Unknown,
        };
        // A scope can retain descendants after its recorded leader has exited.
        // cgroup.events includes child cgroups, unlike a direct PID listing.
        match fs::read_to_string(scope.cgroup().join("cgroup.events")) {
            Ok(events) if events.lines().any(|line| line == "populated 0") => ProcessLiveness::Gone,
            Ok(events) if events.lines().any(|line| line == "populated 1") => {
                log::warn!(
                    "withdrawn call task {} still has descendants in scope {}",
                    metadata.task_id,
                    scope.cgroup().display()
                );
                ProcessLiveness::Alive
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // The scope may have disappeared between lookup and reading its events.
                match scope.cgroup().try_exists() {
                    Ok(false) => ProcessLiveness::Gone,
                    _ => ProcessLiveness::Unknown,
                }
            }
            _ => ProcessLiveness::Unknown,
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn recover_persisted_task_scope(
        &self,
        metadata: &PersistedTask,
    ) -> Result<Option<ScopeLookup>, String> {
        let Some(pid) = metadata.child_pid else {
            return Ok(None);
        };
        let answer = if let Some(start) = metadata.child_start_time_ms {
            self.inner
                .task_scopes
                .adopt(&metadata.task_id, pid, start)?
        } else {
            match lookup_state(&metadata.task_id)? {
                ScopeLookup::Present(_) => {
                    return Err("task scope replay has no exact child identity".into())
                }
                other => other,
            }
        };
        Ok(Some(answer))
    }
}
