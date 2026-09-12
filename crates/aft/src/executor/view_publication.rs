//! Detached view construction. Only CAS and handle installation use the actor
//! epoch; the build owns no executor worker or reader/writer reservation.
use super::JobCancellation;
use crate::context::AppContext;
use parking_lot::{Mutex, RwLock};
use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap},
    path::PathBuf,
    sync::{Arc, LazyLock},
    time::Instant,
};

#[derive(Clone)]
struct Target {
    ctx: Arc<AppContext>,
    epoch: Arc<RwLock<()>>,
}
thread_local! { static CURRENT: RefCell<Option<Target>> = const { RefCell::new(None) }; }

pub(super) struct ActorScope(Option<Target>);
impl ActorScope {
    pub(super) fn install(ctx: Arc<AppContext>, epoch: Arc<RwLock<()>>) -> Self {
        Self(CURRENT.with(|slot| slot.replace(Some(Target { ctx, epoch }))))
    }
}
impl Drop for ActorScope {
    fn drop(&mut self) {
        CURRENT.with(|slot| slot.replace(self.0.take()));
    }
}

struct Running {
    root: PathBuf,
    context: usize,
    token: JobCancellation,
    phase: String,
    started: Instant,
    phase_started: Instant,
    paths: BTreeSet<Vec<u8>>,
}
static JOBS: LazyLock<Mutex<HashMap<u64, Running>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(crate) fn health_snapshot() -> serde_json::Value {
    let jobs = JOBS.lock();
    serde_json::Value::Array(jobs.iter().map(|(id, job)| serde_json::json!({
        "id": id, "kind": "views.publication", "root": job.root, "phase": job.phase,
        "elapsed_ms": job.started.elapsed().as_millis() as u64,
        "phase_ms": job.phase_started.elapsed().as_millis() as u64,
        "cancel_requested": job.token.cancel_requested_before_commit(), "barrier_holder": false,
    })).collect())
}

pub(super) fn running_for_context(ctx: &Arc<AppContext>) -> bool {
    let address = Arc::as_ptr(ctx) as usize;
    JOBS.lock().values().any(|job| job.context == address)
}

struct Lifecycle(u64);
impl Lifecycle {
    fn phase(&self, phase: &str) -> crate::views::Result<()> {
        let mut jobs = JOBS.lock();
        let job = jobs.get_mut(&self.0).expect("live view publication");
        if job.token.cancel_requested_before_commit() {
            return Err(crate::views::ViewError::InvalidManifest(
                "view publication superseded".to_owned(),
            ));
        }
        if job.phase != phase {
            job.phase = phase.to_owned();
            job.phase_started = Instant::now();
        }
        #[cfg(test)]
        let root = job.root.clone();
        drop(jobs);
        #[cfg(test)]
        test_gate(self.0, &root, phase);
        Ok(())
    }
}
impl Drop for Lifecycle {
    fn drop(&mut self) {
        JOBS.lock().remove(&self.0);
    }
}

/// Scheduling succeeds once ownership has moved to the detached worker. Direct
/// standalone callers have no actor epoch and retain the synchronous API.
pub(crate) fn schedule(
    ctx: &AppContext,
    mut paths: BTreeSet<Vec<u8>>,
    allow_blob_put: bool,
) -> Result<(), String> {
    let target = CURRENT.with(|slot| slot.borrow().clone());
    let Some(target) = target.filter(|target| std::ptr::eq(ctx, target.ctx.as_ref())) else {
        return ctx.publish_view_paths(paths, allow_blob_put).map(|_| ());
    };
    let root = ctx
        .canonical_cache_root_opt()
        .ok_or("view root is not configured")?;
    let content_generation = ctx.configure_content_generation();
    let token = JobCancellation::new();
    token.mark_running();
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    {
        let mut jobs = JOBS.lock();
        for job in jobs.values_mut().filter(|job| job.root == root) {
            job.token.request_cancel();
            if paths.is_empty() || job.paths.is_empty() {
                paths.clear();
            } else {
                paths.extend(job.paths.iter().cloned());
            }
        }
        jobs.insert(
            id,
            Running {
                root,
                context: Arc::as_ptr(&target.ctx) as usize,
                token: token.clone(),
                phase: "manifest".to_owned(),
                started: Instant::now(),
                phase_started: Instant::now(),
                paths: paths.clone(),
            },
        );
    }
    let lifecycle = Lifecycle(id);
    std::thread::Builder::new()
        .name("aft-view-publication".to_owned())
        .stack_size(super::EXECUTOR_WORKER_STACK_BYTES)
        .spawn(move || {
            let _cancellation = super::install_job_cancellation(token.clone());
            loop {
                if token.cancel_requested_before_commit()
                    || target.ctx.configure_content_generation() != content_generation
                    || !target.ctx.config().views.enabled
                {
                    break;
                }
                let result = (|| {
                    let _permit = loop {
                        lifecycle
                            .phase("manifest")
                            .map_err(|error| error.to_string())?;
                        if let Some(permit) = target.ctx.cold_build_limiter().try_acquire() {
                            break permit;
                        }
                        token.wait_for_cancellation(std::time::Duration::from_millis(25));
                    };
                    let mut prepared = target.ctx.prepare_view_paths(
                        paths.clone(),
                        allow_blob_put,
                        &mut |phase| lifecycle.phase(phase),
                    )?;
                    // Equivalent unbind/rebind retains this root actor and its epoch.
                    // Content-changing configure is rejected by commit_view_update.
                    let report = {
                        let _epoch = target.epoch.write();
                        lifecycle.phase("cas").map_err(|error| error.to_string())?;
                        // Sealing and cancellation are atomic with respect to superseding
                        // submissions, which hold JOBS while signaling this token.
                        let _jobs = JOBS.lock();
                        if token.cancel_requested_before_commit() {
                            return Err("view publication superseded".to_owned());
                        }
                        let report = target.ctx.commit_view_update(&mut prepared)?;
                        let _ = token.try_seal_committed();
                        report
                    };
                    log::info!(
                    "content-addressed view publication published={} blob_puts={} pending_paths={} root={}",
                    report.published,
                    report.blob_puts,
                    report.pending_paths.len(),
                    target.ctx.canonical_cache_root().display()
                );
                    Ok::<(), String>(())
                })();
                match result {
                    Ok(()) => break,
                    Err(error) => {
                        log::warn!("content-addressed view publication failed: {}", error);
                        // Retain the complete path union until a successful install;
                        // scheduling acknowledgement is not publication completion.
                        token.wait_for_cancellation(std::time::Duration::from_secs(1));
                    }
                }
            }
            drop(lifecycle);
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
#[path = "view_publication_tests.rs"]
mod tests;

#[cfg(test)]
fn test_gate(id: u64, root: &std::path::Path, phase: &str) {
    tests::phase_gate(id, root, phase);
}
