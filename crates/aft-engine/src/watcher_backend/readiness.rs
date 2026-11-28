use std::path::Path;
use std::sync::Arc;

/// The native store-readiness watcher is supplied by the application layer.
pub(crate) struct NativeReadinessWatcherFactory;

impl crate::callgraph_store::ReadinessWatcherFactory for NativeReadinessWatcherFactory {
    fn watch(
        &self,
        dir: &Path,
        handler: Box<dyn FnMut(notify::Result<notify::Event>) + Send>,
    ) -> notify::Result<Box<dyn crate::callgraph_store::ReadinessWatcherHandle>> {
        Ok(Box::new(ForwardingDirectoryWatcher::start(dir, handler)?))
    }
}

impl crate::callgraph_store::ReadinessWatcherHandle for ForwardingDirectoryWatcher {}

/// A native `DirectoryWatcher` whose events go to a callback rather than a
/// channel. A forwarding thread drains its channel without running callback
/// work on the native watcher thread.
pub(crate) struct ForwardingDirectoryWatcher {
    watcher: Option<crate::watcher_backend::DirectoryWatcher>,
    forwarder: Option<std::thread::JoinHandle<()>>,
}

impl ForwardingDirectoryWatcher {
    fn start<F>(dir: &Path, mut handler: F) -> notify::Result<Self>
    where
        F: FnMut(notify::Result<notify::Event>) + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        // The counters are private to this watch: it must not overwrite the
        // project watcher's backend state for the same root.
        let mut watcher = crate::watcher_backend::DirectoryWatcher::new(
            tx,
            Arc::new(crate::context::WatcherCounters::default()),
        );
        watcher.watch(dir, notify::RecursiveMode::NonRecursive)?;
        let forwarder = std::thread::Builder::new()
            .name("aft-readiness-watch".to_string())
            .spawn(move || {
                for event in rx {
                    handler(event);
                }
            })
            .map_err(notify::Error::io)?;
        Ok(Self {
            watcher: Some(watcher),
            forwarder: Some(forwarder),
        })
    }
}

impl Drop for ForwardingDirectoryWatcher {
    fn drop(&mut self) {
        // Stopping the watch drops every sender of the forwarder's channel,
        // so the forwarder finishes the events already sent and returns. It
        // may lock the refresh queue on the way, so this must never run with
        // that lock held.
        drop(self.watcher.take());
        if let Some(forwarder) = self.forwarder.take() {
            let _ = forwarder.join();
        }
    }
}
