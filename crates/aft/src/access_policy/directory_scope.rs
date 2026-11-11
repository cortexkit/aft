//! Bounded directory-consent scope probing, independent of the daemon transport.
use super::{AccessGrant, GrantKind};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

/// Offer a file ask's immediate directory, for reads and writes alike, only
/// when that directory passes `probe_directory_scope_with`: it is not the home
/// folder, a home ancestor, a root-level or volume-level folder, or a protected
/// system tree (including nested system folders). macOS per-user temporary
/// trees remain eligible. Unknown home/volume identity stays file-only.
pub(crate) fn directory_scope(grant: &AccessGrant) -> AccessGrant {
    directory_scope_with(grant, directory_is_root_alias)
}

pub(crate) const DIRECTORY_SCOPE_PROBE_BUDGET: Duration = Duration::from_millis(250);
#[cfg(test)]
pub(crate) static DIRECTORY_SCOPE_PROBE_OVERRIDE_MS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn directory_scope_probe_budget() -> Duration {
    #[cfg(test)]
    {
        let ms = DIRECTORY_SCOPE_PROBE_OVERRIDE_MS.load(Ordering::SeqCst);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    DIRECTORY_SCOPE_PROBE_BUDGET
}

pub(crate) const DIRECTORY_SCOPE_MEMO_TTL: Duration = Duration::from_secs(60);
pub(crate) type DirectoryScopeAnswer = Result<bool, &'static str>;

#[derive(Default)]
pub(crate) struct DirectoryScopeProbe {
    pub(crate) answer: std::sync::Mutex<Option<(Instant, DirectoryScopeAnswer)>>,
    pub(crate) ready: std::sync::Condvar,
}

impl DirectoryScopeProbe {
    pub(crate) fn reusable(&self) -> bool {
        self.answer
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(|(at, answer)| answer.is_ok() && at.elapsed() < DIRECTORY_SCOPE_MEMO_TTL)
    }

    pub(crate) fn wait(&self, deadline: Instant) -> DirectoryScopeAnswer {
        let answer = self.answer.lock().unwrap();
        let (answer, _) = self
            .ready
            .wait_timeout_while(
                answer,
                deadline.saturating_duration_since(Instant::now()),
                |answer| answer.is_none(),
            )
            .unwrap();
        answer
            .filter(|(at, _)| *at <= deadline)
            .map(|(_, answer)| answer)
            .unwrap_or(Err("shared directory probe timed out"))
    }

    pub(crate) fn finish(&self, result: DirectoryScopeAnswer) {
        *self.answer.lock().unwrap() = Some((Instant::now(), result));
        self.ready.notify_all();
    }
}

#[derive(Default)]
pub(crate) struct DirectoryScopeProbes {
    pub(crate) probes: HashMap<PathBuf, Arc<DirectoryScopeProbe>>,
    pub(crate) logged: HashMap<(PathBuf, &'static str), Instant>,
    pub(crate) last_pruned: Option<Instant>,
}

impl DirectoryScopeProbes {
    pub(crate) fn get(&mut self, directory: &Path) -> (Arc<DirectoryScopeProbe>, bool) {
        if self
            .last_pruned
            .is_none_or(|at| at.elapsed() >= DIRECTORY_SCOPE_MEMO_TTL)
        {
            self.probes.retain(|_, probe| probe.reusable());
            self.logged
                .retain(|_, at| at.elapsed() < DIRECTORY_SCOPE_MEMO_TTL);
            self.last_pruned = Some(Instant::now());
        }
        // Ask grants already contain canonical policy identities. Do not resolve
        // the candidate again outside the bounded filesystem helper.
        if let Some(probe) = self.probes.get(directory).filter(|probe| probe.reusable()) {
            return (Arc::clone(probe), false);
        }
        let probe = Arc::new(DirectoryScopeProbe::default());
        self.probes
            .insert(directory.to_path_buf(), Arc::clone(&probe));
        (probe, true)
    }

    pub(crate) fn note_fallback(&mut self, directory: &Path, reason: &'static str) -> bool {
        match self.logged.entry((directory.to_path_buf(), reason)) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if entry.get().elapsed() < DIRECTORY_SCOPE_MEMO_TTL {
                    return false;
                }
                entry.insert(Instant::now());
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Instant::now());
            }
        }
        true
    }
}

pub(crate) static DIRECTORY_SCOPE_PROBES: LazyLock<std::sync::Mutex<DirectoryScopeProbes>> =
    LazyLock::new(|| std::sync::Mutex::new(DirectoryScopeProbes::default()));

pub(crate) fn directory_scope_with<F>(grant: &AccessGrant, root_alias_check: F) -> AccessGrant
where
    F: Fn(&Path, &Path) -> Option<bool> + Send + Sync + 'static,
{
    let deadline = Instant::now() + directory_scope_probe_budget();
    if grant.kind != GrantKind::File {
        return grant.clone();
    }
    let Some(parent) = grant.path.parent() else {
        return grant.clone();
    };
    // One eligibility answer per directory serves read and write asks alike:
    // the directory either may be offered as a whole or it may not.
    let (probe, leader) = DIRECTORY_SCOPE_PROBES.lock().unwrap().get(parent);
    let answer = if leader {
        let result = if let Some(permit) = DIRECTORY_SCOPE_PROBE_LIMITER.acquire_until(deadline) {
            // Keep the slot with filesystem work even after its caller times
            // out, so stalled probes cannot exhaust the shared helper pool.
            crate::bounded_io::run(parent, Some(deadline), move |directory| {
                let _permit = permit;
                Ok(probe_directory_scope_with(&directory, root_alias_check))
            })
            .unwrap_or(Err(
                "directory probe timed out or filesystem helper unavailable",
            ))
        } else {
            Err("directory probe permit timed out")
        };
        // Only an answer delivered within this request's budget is memoized;
        // a helper completing after timeout cannot populate the memo later.
        let result = if Instant::now() <= deadline {
            result
        } else {
            Err("directory probe timed out")
        };
        probe.finish(result);
        result
    } else {
        probe.wait(deadline)
    };
    match answer {
        Ok(true) => AccessGrant {
            path: parent.to_path_buf(),
            kind: GrantKind::Subtree,
            operation: grant.operation,
        },
        Ok(false) => grant.clone(),
        Err(reason) => {
            let should_log = DIRECTORY_SCOPE_PROBES
                .lock()
                .unwrap()
                .note_fallback(parent, reason);
            if should_log {
                log::warn!(
                    "consent directory scope fell back to file-only for {}: {reason}",
                    parent.display()
                );
            }
            grant.clone()
        }
    }
}

pub(crate) fn probe_directory_scope_with(
    parent: &Path,
    root_alias_check: impl Fn(&Path, &Path) -> Option<bool>,
) -> DirectoryScopeAnswer {
    let home = crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|path| {
            #[cfg(all(test, unix))]
            crate::bounded_io::stall_helper_if_requested(&path);
            crate::access_policy::policy_path(&path)
        })
        .ok_or("home identity unknown")?;
    // statfs and stat fail on a directory that is gone, such as task scratch
    // that cleanup removed after its command exited. Name that cause instead
    // of reporting it as an unknown volume.
    if matches!(std::fs::symlink_metadata(parent), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        return Err("directory does not exist");
    }
    let (volume, _) = crate::volume::volume_anchor(parent).ok_or("volume identity unknown")?;
    let volume = crate::access_policy::policy_path(&volume);
    let root = parent.ancestors().last().unwrap_or(parent);
    if !directory_scope_is_narrow(parent, &home, &volume) {
        return Ok(false);
    }
    if directory_is_protected_alias(parent, &home, &volume)
        .ok_or("protected alias identity unknown")?
    {
        return Ok(false);
    }
    if root_alias_check(parent, root).ok_or("root alias identity unknown or scan unavailable")? {
        return Ok(false);
    }
    if root != volume
        && root_alias_check(parent, &volume)
            .ok_or("volume alias identity unknown or scan unavailable")?
    {
        return Ok(false);
    }
    Ok(true)
}

pub(crate) use crate::path_identity::{directory_is_protected_alias, directory_scope_is_narrow};

/// A canonical directory can still be a root-level alias, such as /etc pointing
/// to /private/etc. Only resolvable aliases have a target identity to compare.
/// Reverse aliases cannot be recovered from the candidate's canonical path.
/// Give their enumeration a separate helper and one process-wide slot: unknown
/// or busy scans must never widen a file grant.
pub(crate) const ROOT_ALIAS_SCAN_BUDGET: Duration = Duration::from_millis(250);

pub(crate) struct ScopeProbeLimiter {
    pub(crate) in_flight: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) available: Arc<(std::sync::Mutex<()>, std::sync::Condvar)>,
}

impl ScopeProbeLimiter {
    pub(crate) fn new() -> Self {
        Self {
            in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            available: Arc::default(),
        }
    }

    pub(crate) fn try_acquire(&self) -> Option<ScopeProbePermit> {
        self.in_flight
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .ok()
            .map(|_| ScopeProbePermit(Arc::clone(&self.in_flight), Arc::clone(&self.available)))
    }

    pub(crate) fn acquire_until(&self, deadline: Instant) -> Option<ScopeProbePermit> {
        let (mutex, available) = &*self.available;
        let mut guard = mutex.lock().unwrap();
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            if let Some(permit) = self.try_acquire() {
                return Some(permit);
            }
            (guard, _) = available.wait_timeout(guard, remaining).unwrap();
        }
    }

    pub(crate) fn scan<F>(&self, scan: F) -> Option<bool>
    where
        F: FnOnce() -> Option<bool> + Send + 'static,
    {
        let permit = self.try_acquire()?;

        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("aft-root-alias-scan".into())
            .spawn(move || {
                let result = scan();
                drop(permit);
                let _ = sender.send(result);
            });
        // A failed spawn drops the closure and its permit, which releases the
        // slot; clearing it here too could release a newer caller's slot.
        if spawned.is_err() {
            return None;
        }

        receiver.recv_timeout(ROOT_ALIAS_SCAN_BUDGET).ok().flatten()
    }
}

pub(crate) struct ScopeProbePermit(
    Arc<std::sync::atomic::AtomicBool>,
    Arc<(std::sync::Mutex<()>, std::sync::Condvar)>,
);

impl Drop for ScopeProbePermit {
    fn drop(&mut self) {
        let (mutex, available) = &*self.1;
        let _guard = mutex.lock().unwrap();
        self.0.store(false, std::sync::atomic::Ordering::Release);
        available.notify_all();
    }
}

pub(crate) static DIRECTORY_SCOPE_PROBE_LIMITER: LazyLock<ScopeProbeLimiter> =
    LazyLock::new(ScopeProbeLimiter::new);
pub(crate) static ROOT_ALIAS_SCAN_LIMITER: LazyLock<ScopeProbeLimiter> =
    LazyLock::new(ScopeProbeLimiter::new);

pub(crate) fn directory_is_root_alias(directory: &Path, root: &Path) -> Option<bool> {
    let directory = directory.to_path_buf();
    let root = root.to_path_buf();
    ROOT_ALIAS_SCAN_LIMITER.scan(move || scan_root_alias(&directory, &root))
}

pub(crate) fn scan_root_alias(directory: &Path, root: &Path) -> Option<bool> {
    for entry in std::fs::read_dir(root).ok()? {
        let entry = entry.ok()?;
        if entry.file_type().ok()?.is_symlink() {
            let Ok(target) = entry.path().canonicalize() else {
                continue;
            };
            if crate::access_policy::policy_path(&target) == directory {
                return Some(true);
            }
        }
    }
    Some(false)
}
