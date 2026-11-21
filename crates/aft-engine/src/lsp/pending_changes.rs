//! Document and watched-file changes that wait for the LSP manager
//! lock instead of being dropped.
//!
//! An edit tells the language servers about the new file contents
//! (`didChange`) right after writing it. Those notifications used to be sent
//! only when the manager lock was free at that instant and silently skipped
//! otherwise, so a server kept analysing the old contents until something
//! else resynced the file. They are now merged into one per-context backlog
//! that a single helper thread delivers as soon as the lock frees.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::FileChangeType;

use crate::config::Config;
use crate::lsp::manager::{start_servers_for_file_unlocked, LspManager, WatchedFileStamps};

use super::event_ingress::EventIngress;

/// How many distinct documents may wait for the manager lock. Past this the
/// backlog stops listing documents and instead resyncs every open document
/// whose file changed on disk when it is delivered (see
/// [`LspManager::resync_drifted_open_documents`]).
pub const PENDING_LSP_DOCUMENTS_CAP: usize = 1_024;

/// How many distinct watched-file events may wait for the manager lock.
/// Dropped events are counted against their affected server instances.
pub const PENDING_LSP_WATCHED_CAP: usize = 1_024;

/// The per-context slot holding changes queued for the LSP manager. `Some`
/// exactly while the one helper thread delivering it exists.
pub type PendingLspChangeSlot = Arc<PendingLspChangeQueue>;

pub struct PendingLspChangeQueue {
    queue: parking_lot::Mutex<Option<PendingLspChanges>>,
    ingress: EventIngress,
}

impl PendingLspChangeQueue {
    pub(super) fn new(ingress: EventIngress) -> PendingLspChangeSlot {
        Arc::new(Self {
            queue: parking_lot::Mutex::new(None),
            ingress,
        })
    }

    pub(crate) fn has_worker(&self) -> bool {
        self.queue.lock().is_some()
    }

    pub(super) fn deliver_watched(&self, lsp: &mut LspManager) {
        let extracted = self
            .queue
            .lock()
            .as_mut()
            .map(PendingLspChanges::take_watched);
        if let Some(changes) = extracted {
            changes.apply_watched(lsp);
        } else {
            lsp.recover_watched_overflows();
        }
    }

    pub(super) fn new_backlog(&self) -> PendingLspChanges {
        PendingLspChanges {
            ingress: Some(self.ingress.clone()),
            ..Default::default()
        }
    }

    #[cfg(test)]
    pub(super) fn queue_watched_for_test(&self, events: &[(PathBuf, FileChangeType)]) -> usize {
        let mut queue = self.queue.lock();
        let backlog = queue.get_or_insert_with(|| self.new_backlog());
        backlog.queue_mutation_watched(events, &WatchedFileStamps::new());
        backlog.watched.len()
    }
}

/// Changes merged across callers until the LSP manager lock is free.
///
/// Documents are deduplicated by path and carry no contents: the file is read
/// when the change is delivered, so the servers get its latest contents even
/// when several edits queued meanwhile.
#[derive(Default)]
pub struct PendingLspChanges {
    documents: Vec<PathBuf>,
    seen: HashSet<PathBuf>,
    /// Documents whose change may start a server (a resync after a change
    /// outside AFT); the others go to servers already running.
    start_servers: HashSet<PathBuf>,
    /// Watched-file events, one per path, the latest kind winning.
    watched: Vec<(PathBuf, FileChangeType)>,
    watched_index: HashMap<PathBuf, usize>,
    ingress: Option<EventIngress>,
    /// Mutation events must match each server's registered file globs and kinds;
    /// configuration notifications otherwise retain their existing send behavior.
    mutation_watched: HashSet<PathBuf>,
    watched_stamps: WatchedFileStamps,
    /// Documents were dropped past [`PENDING_LSP_DOCUMENTS_CAP`].
    overflowed: bool,
    /// The latest configuration seen by a queuing caller.
    config: Option<Arc<Config>>,
}

impl PendingLspChanges {
    /// Queue a document whose contents changed on disk.
    pub(crate) fn queue_document(&mut self, path: &Path, start_servers: bool) {
        if self.overflowed {
            return;
        }
        if start_servers {
            self.start_servers.insert(path.to_path_buf());
        }
        if self.seen.insert(path.to_path_buf()) {
            self.documents.push(path.to_path_buf());
        }
        if self.documents.len() > PENDING_LSP_DOCUMENTS_CAP {
            self.overflowed = true;
            self.documents.clear();
            self.seen.clear();
            self.start_servers.clear();
        }
    }

    /// Queue watched-file events, including source-file creation and deletion.
    pub(crate) fn queue_watched(&mut self, events: &[(PathBuf, FileChangeType)]) {
        for (path, typ) in events {
            if let Some(index) = self.watched_index.get(path) {
                self.watched[*index].1 = *typ;
            } else if self.watched.len() < PENDING_LSP_WATCHED_CAP {
                self.watched_index.insert(path.clone(), self.watched.len());
                self.watched.push((path.clone(), *typ));
            } else if let Some(ingress) = &self.ingress {
                let resolved = super::manager::resolve_for_lsp_uri(path);
                ingress.record_watched_drop(path, &resolved);
            }
        }
    }

    pub(crate) fn queue_watched_with_stamps(
        &mut self,
        events: &[(PathBuf, FileChangeType)],
        stamps: &WatchedFileStamps,
    ) {
        self.queue_watched(events);
        for (path, _) in events {
            if self.watched_index.contains_key(path) {
                if let Some(stamp) = stamps.get(path) {
                    self.watched_stamps.insert(path.clone(), *stamp);
                } else {
                    self.watched_stamps.remove(path);
                }
            }
        }
    }

    pub(crate) fn queue_mutation_watched(
        &mut self,
        events: &[(PathBuf, FileChangeType)],
        stamps: &WatchedFileStamps,
    ) {
        self.queue_watched_with_stamps(events, stamps);
        self.mutation_watched.extend(
            events.iter().filter_map(|(path, _)| {
                self.watched_index.contains_key(path).then(|| path.clone())
            }),
        );
    }

    fn set_config(&mut self, config: Arc<Config>) {
        self.config = Some(config);
    }

    fn is_empty(&self) -> bool {
        self.documents.is_empty() && self.watched.is_empty() && !self.overflowed
    }

    /// The queued documents that may start a server, so the caller can start
    /// those servers before taking the manager lock.
    fn documents_starting_servers(&self) -> impl Iterator<Item = &PathBuf> {
        self.documents
            .iter()
            .filter(|path| self.start_servers.contains(*path))
    }

    /// Deliver the backlog: configuration events first (as an edit does),
    /// then each document's current contents.
    fn take_watched(&mut self) -> Self {
        self.watched_index.clear();
        Self {
            watched: std::mem::take(&mut self.watched),
            mutation_watched: std::mem::take(&mut self.mutation_watched),
            watched_stamps: std::mem::take(&mut self.watched_stamps),
            config: self.config.clone(),
            ..Default::default()
        }
    }

    fn apply_watched(self, lsp: &mut LspManager) {
        lsp.recover_watched_overflows();
        let config = self.config.unwrap_or_default();
        let (mutations, watched): (Vec<_>, Vec<_>) = self
            .watched
            .into_iter()
            .partition(|(path, _)| self.mutation_watched.contains(path));
        let markers = config
            .lsp_servers
            .iter()
            .flat_map(|server| server.root_markers.iter().cloned())
            .collect::<Vec<_>>();
        lsp.forward_watcher_file_events_with_stamps(&mutations, &markers, &self.watched_stamps);
        if !watched.is_empty() {
            if let Err(error) =
                lsp.notify_files_watched_changed_with_stamps(&watched, &self.watched_stamps)
            {
                crate::slog_warn!("queued watched-file sync error: {error}");
            }
        }
    }

    pub(crate) fn apply(mut self, lsp: &mut LspManager) {
        self.take_watched().apply_watched(lsp);
        let config = self.config.unwrap_or_default();
        for path in &self.documents {
            // A file deleted meanwhile has nothing to send; the watcher
            // reports the deletion.
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            let sent = if self.start_servers.contains(path) {
                lsp.notify_file_changed(path, &content, &config)
            } else {
                lsp.notify_file_changed_if_running(path, &content, &config)
            };
            if let Err(error) = sent {
                crate::slog_warn!("queued sync error for {}: {error}", path.display());
            }
        }
        if self.overflowed {
            lsp.resync_drifted_open_documents(&config);
        }
    }
}

/// Whether a change reached the servers now or waits for the manager lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspChangeDelivery {
    /// Sent while the caller held the lock.
    Sent,
    /// Queued; the helper thread sends it once the lock frees.
    Queued,
}

/// Send a change now if the manager lock is free (`send_now`), and otherwise
/// queue it (`queue`) for the one helper thread that waits for the lock. A
/// backlog that already exists is joined rather than overtaken, so changes
/// reach the servers in the order they were made.
pub fn send_or_queue_lsp_change(
    manager: &Arc<parking_lot::Mutex<LspManager>>,
    slot: &PendingLspChangeSlot,
    config: Arc<Config>,
    send_now: impl FnOnce(&mut LspManager),
    queue: impl FnOnce(&mut PendingLspChanges),
) -> LspChangeDelivery {
    {
        let mut pending = slot.queue.lock();
        if let Some(backlog) = pending.as_mut() {
            queue(backlog);
            backlog.set_config(config);
            return LspChangeDelivery::Queued;
        }
    }
    if let Some(mut lsp) = manager.try_lock() {
        send_now(&mut lsp);
        return LspChangeDelivery::Sent;
    }
    queue_lsp_change(manager, slot, config, queue)
}

/// Queue even when the manager is free. A caller with an expired diagnostics
/// budget must not spend another handshake or stdin-write budget delivering
/// the change itself; the coalesced helper reads the latest disk contents.
pub(crate) fn queue_lsp_change(
    manager: &Arc<parking_lot::Mutex<LspManager>>,
    slot: &PendingLspChangeSlot,
    config: Arc<Config>,
    queue: impl FnOnce(&mut PendingLspChanges),
) -> LspChangeDelivery {
    let mut pending = slot.queue.lock();
    if let Some(backlog) = pending.as_mut() {
        queue(backlog);
        backlog.set_config(config);
        return LspChangeDelivery::Queued;
    }
    let mut backlog = slot.new_backlog();
    queue(&mut backlog);
    backlog.set_config(config);
    *pending = Some(backlog);
    drop(pending);
    let manager = Arc::clone(manager);
    let helper_slot = Arc::clone(slot);
    if let Err(error) = std::thread::Builder::new()
        .name("aft-lsp-pending-changes".into())
        .spawn(move || run_pending_change_helper(&manager, &helper_slot))
    {
        // Free the slot so a later change can try again.
        slot.queue.lock().take();
        crate::slog_warn!("could not queue a document change for the LSP servers: {error}");
    }
    LspChangeDelivery::Queued
}

/// Body of the one helper thread serving a [`PendingLspChangeSlot`]: take
/// what is queued, start any server a resync needs without the manager lock,
/// deliver the rest under it, and repeat while callers queued more. The slot
/// stays `Some` (emptied) while a batch is delivered, so later changes join
/// the slot instead of overtaking the batch; clearing it once nothing is left
/// lets the next contended change start a new helper.
pub(crate) fn run_pending_change_helper(
    manager: &Arc<parking_lot::Mutex<LspManager>>,
    slot: &PendingLspChangeSlot,
) {
    loop {
        let backlog = {
            // Keep watched changes available to a diagnostic reader that
            // acquires the manager before this helper does.
            let mut manager = manager.lock();
            manager.deliver_pending_watched_backlog();
            let mut pending = slot.queue.lock();
            let Some(backlog) = pending.as_mut() else {
                return;
            };
            if backlog.is_empty() {
                *pending = None;
                return;
            }
            let config = backlog.config.clone();
            let taken = std::mem::replace(backlog, slot.new_backlog());
            backlog.config = config;
            let mut taken = taken;
            drop(pending);
            taken.take_watched().apply_watched(&mut manager);
            taken
        };
        let config = backlog.config.clone().unwrap_or_default();
        for path in backlog.documents_starting_servers() {
            start_servers_for_file_unlocked(|| manager.lock(), path, &config);
        }
        backlog.apply(&mut manager.lock());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_are_deduplicated_and_overflow_switches_to_a_drift_resync() {
        let mut backlog = PendingLspChanges::default();
        backlog.queue_document(Path::new("/a.rs"), false);
        backlog.queue_document(Path::new("/a.rs"), true);
        assert_eq!(backlog.documents, vec![PathBuf::from("/a.rs")]);
        assert!(backlog.start_servers.contains(Path::new("/a.rs")));

        for index in 0..=PENDING_LSP_DOCUMENTS_CAP {
            backlog.queue_document(&PathBuf::from(format!("/f{index}.rs")), false);
        }
        assert!(backlog.overflowed);
        assert!(backlog.documents.is_empty());
        assert!(!backlog.is_empty(), "an overflowed backlog still has work");
    }

    #[test]
    fn watched_events_keep_the_latest_kind_per_path() {
        let mut backlog = PendingLspChanges::default();
        backlog.queue_watched(&[(PathBuf::from("/Cargo.toml"), FileChangeType::DELETED)]);
        backlog.queue_watched(&[(PathBuf::from("/Cargo.toml"), FileChangeType::CREATED)]);
        assert_eq!(
            backlog.watched,
            vec![(PathBuf::from("/Cargo.toml"), FileChangeType::CREATED)]
        );
    }
}
