//! Best-effort commit attribution, separate from the GitHub write relay.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use subc_protocol::scope::ScopeStamp;

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

fn agent_id(scope: &ScopeStamp) -> Option<&str> {
    scope
        .attributes
        .on_behalf_of_agent
        .as_deref()
        .or(scope.attributes.agent_id.as_deref())
        .filter(|id| !id.is_empty())
}

pub(crate) struct ScopeGuard(Option<String>);

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        AGENT.with(|agent| agent.replace(self.0.take()));
    }
}

pub(crate) fn enter(scope: Option<&ScopeStamp>) -> ScopeGuard {
    ScopeGuard(AGENT.with(|agent| agent.replace(scope.and_then(agent_id).map(str::to_owned))))
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
        "ref":"test-run","scope_epoch":1,"kind":"ephemeral","owner_authorized":true,
        "attributes": attributes
    }))
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
