//! Best-effort commit attribution, separate from the GitHub write relay.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use subc_protocol::scope::{ParentState, ScopeKind, ScopeStamp};

const TTL: Duration = Duration::from_secs(10 * 60);
const FIRST_SHELL_WAIT: Duration = Duration::from_secs(1);

thread_local! {
    static AGENT: RefCell<Option<String>> = const { RefCell::new(None) };
}

static SOURCE: Mutex<Option<Arc<Cache>>> = Mutex::new(None);

type Lookup = dyn Fn(&str) -> Result<Option<String>, ()> + Send + Sync;

#[derive(Default)]
struct Entry {
    answer: Option<(Instant, Option<String>)>,
    refreshing: bool,
}

struct Cache {
    lookup: Box<Lookup>,
    entries: Mutex<HashMap<String, Entry>>,
    ready: Condvar,
}

impl Cache {
    fn new(
        lookup: impl Fn(&str) -> Result<Option<String>, ()> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            lookup: Box::new(lookup),
            entries: Mutex::new(HashMap::new()),
            ready: Condvar::new(),
        })
    }

    fn line(self: &Arc<Self>, agent: &str) -> Option<String> {
        let start = Instant::now();
        let mut entries = self.entries.lock().ok()?;
        let entry = entries.entry(agent.to_owned()).or_default();
        if entry
            .answer
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() < TTL)
        {
            return entry.answer.as_ref()?.1.clone();
        }
        let had_answer = entry.answer.is_some();
        if entry
            .answer
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() >= TTL * 2)
        {
            entry.answer = None;
        }
        if !entry.refreshing {
            entry.refreshing = true;
            let cache = Arc::clone(self);
            let agent = agent.to_owned();
            if std::thread::Builder::new()
                .name("aft-git-coauthor".into())
                .spawn(move || {
                    let answer = (cache.lookup)(&agent);
                    if let Ok(mut entries) = cache.entries.lock() {
                        let entry = entries.entry(agent).or_default();
                        if let Ok(line) = answer {
                            entry.answer = Some((
                                Instant::now(),
                                line.filter(|line| {
                                    !line.trim().is_empty() && !line.contains(['\n', '\r', '\0'])
                                }),
                            ));
                        }
                        entry.refreshing = false;
                        cache.ready.notify_all();
                    }
                })
                .is_err()
            {
                entry.refreshing = false;
            }
        }
        // Old answers remain usable during revalidation, but only the cold
        // lookup may hold up shell launch.
        if let Some((_, line)) = &entry.answer {
            return line.clone();
        }
        if had_answer {
            return None;
        }
        let (entries, _) = self
            .ready
            .wait_timeout_while(
                entries,
                FIRST_SHELL_WAIT.saturating_sub(start.elapsed()),
                |entries| entries.get(agent).is_some_and(|entry| entry.refreshing),
            )
            .ok()?;
        entries.get(agent)?.answer.as_ref()?.1.clone()
    }
}

pub fn install(connection_file: PathBuf) {
    let cache = Cache::new(move |agent| {
        let transport = crate::agent_data::transport::ModuleTransport::entorhinal(
            connection_file.clone(),
            subc_protocol::BindIdentity::new("aft", "aft-git-coauthor"),
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| ())?
            .block_on(transport.github_coauthor(agent))
    });
    *SOURCE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cache);
}

#[doc(hidden)]
pub fn agent_id<'a>(
    scope: &'a ScopeStamp,
    worker_parent: Option<&'a ScopeStamp>,
    head_candidates: impl IntoIterator<Item = &'a ScopeStamp>,
) -> Option<&'a str> {
    let present = |value: Option<&'a str>| value.filter(|id| !id.is_empty());
    if !scope.owner_authorized {
        return None;
    }
    match scope.kind {
        ScopeKind::Head => present(scope.attributes.agent_id.as_deref()),
        ScopeKind::Ephemeral if present(scope.attributes.run_id.as_deref()).is_some() => {
            match (
                present(scope.attributes.on_behalf_of_agent.as_deref()),
                present(scope.attributes.agent_id.as_deref()),
            ) {
                (Some(sponsor), None) => {
                    let worker = worker_parent.filter(|worker| {
                        worker.kind == ScopeKind::Worker && linked_to(scope, worker)
                    })?;
                    let head = worker_head(worker, head_candidates)?;
                    (sponsor == head).then_some(head)
                }
                (None, Some(agent)) => Some(agent),
                _ => None,
            }
        }
        ScopeKind::Worker => worker_head(scope, head_candidates),
        _ => None,
    }
}

fn linked_to(child: &ScopeStamp, parent: &ScopeStamp) -> bool {
    child.parent_state == Some(ParentState::Linked)
        && parent.owner_authorized
        && child.parent.as_ref().is_some_and(|link| {
            parent.owner == link.owner
                && parent.scope_ref == link.scope_ref
                && parent.scope_epoch == link.scope_epoch
        })
}

fn worker_head<'a>(
    worker: &ScopeStamp,
    candidates: impl IntoIterator<Item = &'a ScopeStamp>,
) -> Option<&'a str> {
    let mut linked = candidates
        .into_iter()
        .filter(|head| head.kind == ScopeKind::Head && linked_to(worker, head));
    let head = linked
        .next()?
        .attributes
        .agent_id
        .as_deref()
        .filter(|id| !id.is_empty())?;
    if linked.next().is_some() {
        return None;
    }
    if worker
        .attributes
        .on_behalf_of_agent
        .as_deref()
        .is_some_and(|sponsor| !sponsor.is_empty() && sponsor != head)
    {
        return None;
    }
    Some(head)
}

pub(crate) struct ScopeGuard(Option<String>);

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        AGENT.with(|agent| agent.replace(self.0.take()));
    }
}

pub(crate) fn enter(agent_id: Option<&str>) -> ScopeGuard {
    ScopeGuard(AGENT.with(|agent| agent.replace(agent_id.map(str::to_owned))))
}

pub(super) fn line() -> Option<String> {
    let agent = AGENT.with(|agent| agent.borrow().clone())?;
    #[cfg(test)]
    if let Some(cache) = FIXTURE.with(|fixture| fixture.borrow().clone()) {
        return cache.line(&agent);
    }
    let cache = SOURCE.lock().ok()?.clone()?;
    cache.line(&agent)
}

#[cfg(test)]
thread_local! {
    static FIXTURE: RefCell<Option<Arc<Cache>>> = const { RefCell::new(None) };
}

#[cfg(all(test, unix))]
pub(super) fn with_fixture<T>(
    lookup: impl Fn(&str) -> Result<Option<String>, ()> + Send + Sync + 'static,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<Arc<Cache>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            FIXTURE.with(|fixture| fixture.replace(self.0.take()));
        }
    }
    let _restore = Restore(FIXTURE.with(|fixture| fixture.replace(Some(Cache::new(lookup)))));
    run()
}

#[cfg(all(test, unix))]
pub(super) fn scope_fixture(attributes: serde_json::Value) -> ScopeStamp {
    serde_json::from_value(serde_json::json!({
        "owner": {"kind":"reserved","module_id":"test-module"},
        "ref":"test-run","scope_epoch":1,"kind":"head","owner_authorized":true,
        "attributes": attributes
    }))
    .unwrap()
}

#[cfg(all(test, unix))]
pub(super) fn link_fixture(child: &mut ScopeStamp, parent: &ScopeStamp) {
    child.parent = Some(
        serde_json::from_value(serde_json::json!({
            "owner":parent.owner,"ref":parent.scope_ref,"scope_epoch":parent.scope_epoch
        }))
        .unwrap(),
    );
    child.parent_state = Some(ParentState::Linked);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[cfg(unix)]
    #[test]
    fn git_hook_coauthor_never_uses_the_classic_workers_own_agent_id() {
        let mut worker = scope_fixture(serde_json::json!({"agent_id":"worker-identity"}));
        worker.kind = subc_protocol::scope::ScopeKind::Worker;
        assert_eq!(agent_id(&worker, None, []), None);
        let head = scope_fixture(
            serde_json::json!({"agent_id":"head-identity", "on_behalf_of_agent":"not-the-head"}),
        );
        link_fixture(&mut worker, &head);
        assert_eq!(agent_id(&head, None, []), Some("head-identity"));
        assert_eq!(agent_id(&worker, None, [&head]), Some("head-identity"));
    }

    #[cfg(unix)]
    #[test]
    fn git_hook_coauthor_requires_a_linked_head_that_agrees_with_sponsorship() {
        let head = scope_fixture(serde_json::json!({"agent_id":"head-identity"}));
        let mut worker = scope_fixture(serde_json::json!({
            "agent_id":"worker-identity", "on_behalf_of_agent":"different-agent"
        }));
        worker.kind = ScopeKind::Worker;
        link_fixture(&mut worker, &head);
        assert_eq!(agent_id(&worker, None, [&head]), None);
        worker.attributes.on_behalf_of_agent = Some("head-identity".into());
        assert_eq!(agent_id(&worker, None, [&head]), Some("head-identity"));
        assert_eq!(agent_id(&worker, None, []), None);
        worker.attributes.on_behalf_of_agent = None;
        worker.parent_state = Some(ParentState::Pending);
        assert_eq!(agent_id(&worker, None, [&head]), None);
        link_fixture(&mut worker, &head);
        let mut other = head.clone();
        other.attributes.agent_id = Some("different-head".into());
        assert_eq!(agent_id(&worker, None, [&head, &other]), None);
    }

    #[cfg(unix)]
    #[test]
    fn git_hook_coauthor_worker_run_needs_both_links_and_an_agreeing_sponsor() {
        let head = scope_fixture(serde_json::json!({"agent_id":"head-identity"}));
        let mut worker = scope_fixture(serde_json::json!({"agent_id":"worker-identity"}));
        worker.kind = ScopeKind::Worker;
        worker.scope_ref = "test-worker".into();
        link_fixture(&mut worker, &head);
        let mut run = scope_fixture(
            serde_json::json!({"run_id":"test-program","on_behalf_of_agent":"head-identity"}),
        );
        run.kind = ScopeKind::Ephemeral;
        link_fixture(&mut run, &worker);
        assert_eq!(
            agent_id(&run, Some(&worker), [&head]),
            Some("head-identity")
        );
        assert_eq!(agent_id(&run, None, [&head]), None);
        run.attributes.on_behalf_of_agent = Some("wrong-sponsor".into());
        assert_eq!(agent_id(&run, Some(&worker), [&head]), None);
        run.attributes.on_behalf_of_agent = Some("head-identity".into());
        run.parent_state = Some(ParentState::Ended);
        assert_eq!(agent_id(&run, Some(&worker), [&head]), None);
    }

    #[test]
    fn git_hook_coauthor_drops_the_answer_after_one_stale_ttl() {
        let cache = Cache::new(|_| Err(()));
        let agent = "agent-refresh-failed";
        {
            let mut entries = cache.entries.lock().unwrap();
            entries.entry(agent.into()).or_default().answer = Some((
                Instant::now() - TTL,
                Some("Old Bot <old@example.test>".into()),
            ));
        }
        assert_eq!(
            cache.line(agent).as_deref(),
            Some("Old Bot <old@example.test>")
        );
        {
            let mut entries = cache.entries.lock().unwrap();
            entries.get_mut(agent).unwrap().answer.as_mut().unwrap().0 -= TTL;
        }
        assert_eq!(cache.line(agent), None);
    }

    #[test]
    fn git_hook_coauthor_cache_hits_and_refreshes_an_expired_answer_in_background() {
        let reads = Arc::new(AtomicUsize::new(0));
        let cache = Cache::new({
            let reads = reads.clone();
            move |_| {
                Ok(Some(format!(
                    "Bot {} <bot@example.test>",
                    reads.fetch_add(1, Ordering::Relaxed)
                )))
            }
        });
        let first = "Bot 0 <bot@example.test>";
        assert_eq!(cache.line("agent-cache").as_deref(), Some(first));
        assert_eq!(cache.line("agent-cache").as_deref(), Some(first));
        assert_eq!(reads.load(Ordering::Relaxed), 1);
        let mut entries = cache.entries.lock().unwrap();
        entries
            .get_mut("agent-cache")
            .unwrap()
            .answer
            .as_mut()
            .unwrap()
            .0 -= TTL;
        drop(entries);
        assert_eq!(cache.line("agent-cache").as_deref(), Some(first));
        let (entries, _) = cache
            .ready
            .wait_timeout_while(cache.entries.lock().unwrap(), FIRST_SHELL_WAIT, |entries| {
                entries["agent-cache"].refreshing
            })
            .unwrap();
        drop(entries);
        assert_eq!(
            cache.line("agent-cache").as_deref(),
            Some("Bot 1 <bot@example.test>")
        );
        assert_eq!(reads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn git_hook_coauthor_cold_lookup_times_out_in_one_second() {
        let (release, blocked) = std::sync::mpsc::channel();
        let blocked = Mutex::new(blocked);
        let cache = Cache::new(move |_| {
            blocked.lock().unwrap().recv().unwrap();
            Ok(Some("Late Bot <late@example.test>".into()))
        });
        let start = Instant::now();
        assert_eq!(cache.line("agent-slow"), None);
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(950), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
        release.send(()).unwrap();
        assert_eq!(
            cache.line("agent-slow").as_deref(),
            Some("Late Bot <late@example.test>")
        );
    }
}
