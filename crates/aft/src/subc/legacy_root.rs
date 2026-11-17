use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex as StdMutex, OnceLock};

use serde_json::{json, Value};
use subc_protocol::{ErrorBody, Frame};

#[cfg(test)]
use super::scope_workdir_tests;
use super::RouteChannel;
use crate::path_identity::ProjectRootId;

pub(super) fn normalized_scope_workdir(workdir: &Path) -> Option<PathBuf> {
    let mut ancestor = workdir;
    let mut missing = Vec::new();
    loop {
        match ProjectRootId::from_path(ancestor) {
            Ok(root) => {
                let mut normalized = root.into_path_buf();
                for component in missing.iter().rev() {
                    normalized.push(component);
                }
                return Some(normalized);
            }
            Err(crate::path_identity::IdentityError::NonExistentPath { .. }) => {
                missing.push(ancestor.file_name()?.to_os_string());
                ancestor = ancestor.parent()?;
            }
            Err(_) => return None,
        }
    }
}

pub(super) fn resolve_route_project_root(
    legacy_root: Option<&Path>,
    scope: Option<&subc_protocol::scope::ScopeStamp>,
) -> Result<Option<PathBuf>, &'static str> {
    let stamped_workdir = scope.and_then(|stamp| stamp.workdir.as_deref());
    let root = stamped_workdir.map(Path::new).or(legacy_root);
    match root {
        Some(root) if root.as_os_str() == "/" => Ok(None),
        Some(root) if root.is_absolute() => {
            let normalized = normalized_scope_workdir(root).unwrap_or_else(|| root.to_path_buf());
            // The daemon uses / when an opener has no project. Aliases of that
            // sentinel must not accidentally configure or index the filesystem.
            if normalized.as_os_str() == "/" {
                Ok(None)
            } else {
                Ok(Some(if stamped_workdir.is_some() {
                    normalized
                } else {
                    root.to_path_buf()
                }))
            }
        }
        Some(_) => Err(PROJECT_REQUIRED),
        None => Ok(None),
    }
}

pub(super) const PROJECT_REQUIRED: &str =
    "This connection has no project folder, so AFT cannot run project tools on it.";
pub(super) const PROJECT_REQUIRED_CODE: &str = "project_required";

pub(super) fn project_folder_refusal(
    code: &str,
    workdir: Option<&str>,
    legacy_root: Option<&Path>,
    route: RouteChannel,
    corr: u64,
) -> ErrorBody {
    let (source, folder) = match workdir {
        Some(workdir) => ("scope_workdir", Some(workdir.to_owned())),
        None => (
            if legacy_root.is_some() {
                "legacy_bind"
            } else {
                "none"
            },
            legacy_root.map(|root| root.to_string_lossy().into_owned()),
        ),
    };
    let cause = if code == "invalid_workdir" {
        "invalid_workdir"
    } else if folder.is_some() {
        "filesystem_root"
    } else {
        "project_folder_missing"
    };
    let detail = json!({
        "cause": cause,
        "source": source,
        "folder": folder,
        "requirement": "absolute_non_root_project_directory",
    });
    crate::slog_warn!(
        "subc project folder refused: code={} route={} corr={} detail={}",
        code,
        route,
        corr,
        detail,
    );
    ErrorBody::new(code, PROJECT_REQUIRED).with_detail(detail)
}

pub(super) fn legacy_route_project_root(frame: &Frame) -> Option<PathBuf> {
    serde_json::from_slice::<Value>(&frame.body)
        .ok()
        .and_then(|body| body["identity"]["project_root"].as_str().map(PathBuf::from))
}

pub(super) fn log_route_project_key_change(
    identity: &subc_protocol::BindIdentity,
    legacy_root: Option<&Path>,
    project_root: &Path,
    route: RouteChannel,
) {
    // Catalog and consent keys use literal display strings. Compare those strings
    // so aliases that canonicalize to one folder still produce a key-change log.
    let Some(legacy_root) = legacy_root else {
        return;
    };
    let bind_key = legacy_root.display().to_string();
    let resolved_key = project_root.display().to_string();
    if bind_key != resolved_key {
        crate::slog_info!(
            "subc route project key changed: bind_project_root={} resolved_project_root={} session={} route={}",
            bind_key,
            resolved_key,
            identity.session,
            route,
        );
    }
}

pub(super) const MAX_SCOPE_WORKDIR_DIAGNOSTICS: usize = 2;

#[derive(Default)]
pub(super) struct ScopeWorkdirDiagnostics {
    pub(super) in_flight: StdMutex<HashSet<String>>,
    pub(super) pool: OnceLock<crate::bounded_io::IsolatedPool>,
    #[cfg(test)]
    pub(super) idle: std::sync::Condvar,
}

pub(super) static SCOPE_WORKDIR_DIAGNOSTICS: LazyLock<Arc<ScopeWorkdirDiagnostics>> =
    LazyLock::new(|| Arc::new(ScopeWorkdirDiagnostics::default()));

pub(super) struct ScopeWorkdirPermit {
    pub(super) diagnostics: Arc<ScopeWorkdirDiagnostics>,
    pub(super) workdir: String,
}

impl ScopeWorkdirDiagnostics {
    pub(super) fn try_admit(
        self: &Arc<Self>,
        workdir: &str,
    ) -> Result<ScopeWorkdirPermit, &'static str> {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if in_flight.contains(workdir) {
            return Err("workdir_already_in_flight");
        }
        if in_flight.len() >= MAX_SCOPE_WORKDIR_DIAGNOSTICS {
            return Err("diagnostic_capacity_exhausted");
        }
        // The daemon supplies the canonical spelling, including missing tails.
        // Admission must not touch the filesystem to derive a different key.
        in_flight.insert(workdir.to_owned());
        Ok(ScopeWorkdirPermit {
            diagnostics: Arc::clone(self),
            workdir: workdir.to_owned(),
        })
    }
}

impl Drop for ScopeWorkdirPermit {
    fn drop(&mut self) {
        self.diagnostics
            .in_flight
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.workdir);
        #[cfg(test)]
        self.diagnostics.idle.notify_all();
    }
}

pub(super) fn schedule_scope_workdir_comparison(
    identity: &subc_protocol::BindIdentity,
    legacy_root: Option<&Path>,
    scope: Option<&subc_protocol::scope::ScopeStamp>,
) {
    let Some(legacy_root) = legacy_root else {
        return;
    };
    let Some(stamp) = scope else { return };
    let Some(workdir) = stamp.workdir.as_deref() else {
        return;
    };
    #[cfg(test)]
    let observed = scope_workdir_tests::take_comparison_observer();
    let permit = match SCOPE_WORKDIR_DIAGNOSTICS.try_admit(workdir) {
        Ok(permit) => permit,
        Err(reason) => {
            log_scope_workdir_comparison(
                identity,
                legacy_root,
                stamp,
                Err(io::Error::other(reason)),
            );
            #[cfg(test)]
            if let Some(observed) = observed {
                let _ = observed.send(());
            }
            return;
        }
    };
    let budget = crate::bounded_io::Budget::after(crate::bounded_io::IO_BUDGET);
    let deadline = tokio::time::Instant::from_std(budget.deadline);
    let project_root = legacy_root.to_path_buf();
    let legacy_root = legacy_root.to_path_buf();
    let workdir = workdir.to_owned();
    // A diagnostic must not delay frame processing or serialize project work. The
    // bounded helper owns all filesystem work; the blocking task only waits for it.
    let worker = tokio::task::spawn_blocking(move || {
        compare_scope_workdir(project_root, workdir, budget, permit)
    });
    let identity = identity.clone();
    let stamp = stamp.clone();
    tokio::spawn(async move {
        // Include time queued in Tokio's blocking pool in the same budget. Late
        // filesystem results are discarded and cannot produce a mismatch record.
        let result = match tokio::time::timeout_at(deadline, worker).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => Err(io::Error::other(format!(
                "comparison worker failed: {error}"
            ))),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "filesystem comparison budget expired",
            )),
        };
        log_scope_workdir_comparison(&identity, &legacy_root, &stamp, result);
        #[cfg(test)]
        if let Some(observed) = observed {
            let _ = observed.send(());
        }
    });
}

pub(super) fn compare_scope_workdir(
    project_root: PathBuf,
    workdir: String,
    budget: crate::bounded_io::Budget,
    permit: ScopeWorkdirPermit,
) -> io::Result<Option<(PathBuf, PathBuf)>> {
    // Either folder can have a missing tail. Normalize each existing prefix
    // like a routing root, including Windows path spelling, then append that tail.
    // Bound both walks so a diagnostic never waits indefinitely on filesystem I/O.
    let pool = permit
        .diagnostics
        .pool
        .get_or_init(|| crate::bounded_io::IsolatedPool::new(MAX_SCOPE_WORKDIR_DIAGNOSTICS))
        .clone();
    crate::bounded_io::with_budget(Some(budget), || {
        pool.run(Path::new(&workdir), None, move |workdir| {
            // Keep both the capacity slot and this folder's deduplication key
            // until the actual filesystem job exits, not merely until timeout.
            let _permit = permit;
            let (Some(project_root), Some(workdir)) = (
                normalized_scope_workdir(&project_root),
                normalized_scope_workdir(&workdir),
            ) else {
                return Ok(None);
            };
            Ok((workdir != project_root).then_some((project_root, workdir)))
        })
    })
}

pub(super) fn log_scope_workdir_comparison(
    identity: &subc_protocol::BindIdentity,
    legacy_root: &Path,
    stamp: &subc_protocol::scope::ScopeStamp,
    result: io::Result<Option<(PathBuf, PathBuf)>>,
) {
    match result {
        Ok(Some((project_root, workdir))) => crate::slog_warn!(
            "subc scope workdir differs: project_root={} workdir={} session={} project_id={}",
            project_root.display(),
            workdir.display(),
            identity.session,
            stamp.project_id.as_deref().unwrap_or("none"),
        ),
        Ok(None) => {}
        Err(error) => crate::slog_info!(
            "subc scope workdir comparison skipped: project_root={} workdir={} session={} project_id={} reason={}",
            legacy_root.display(),
            stamp.workdir.as_deref().unwrap_or("none"),
            identity.session,
            stamp.project_id.as_deref().unwrap_or("none"),
            error,
        ),
    }
}
