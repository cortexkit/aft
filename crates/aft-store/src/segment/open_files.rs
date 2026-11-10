//! The segment files a process holds open, never more than a fixed number
//! at once.
//!
//! Every open [`Segment`](super::Segment) reads its file through a table of
//! open files instead of holding a descriptor of its own. A daemon keeps
//! open every segment that a bound checkout's publication names, and a
//! store whose segments pile up names thousands; with one descriptor each,
//! a process ran into macOS's limit of 10,240 open files, past which
//! spawning a shell command fails. Once the table is full, opening another
//! file closes the one read least recently, and that segment opens its file
//! again on its next read, checking that it is still the same segment.

use std::collections::HashMap;
use std::fs::File;
use std::sync::{Arc, Mutex, OnceLock};

use crate::cache::FileToken;

/// How many segment files a process holds open at most: a tenth of macOS's
/// per-process limit, leaving the rest to everything else a daemon opens.
/// Segments past it still read, each opening its file again when it is
/// read after being closed.
pub const MAX_OPEN_SEGMENT_FILES: usize = 1024;

/// A table of open segment files, keyed by each open segment's token.
pub(crate) struct OpenFiles {
    cap: usize,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    files: HashMap<FileToken, Held>,
    /// Counts uses, so the file used least recently has the lowest `used`.
    clock: u64,
}

struct Held {
    file: Arc<File>,
    used: u64,
}

impl State {
    /// `token`'s open file, marked as used now.
    fn touch(&mut self, token: FileToken) -> Option<Arc<File>> {
        self.clock += 1;
        let clock = self.clock;
        self.files.get_mut(&token).map(|held| {
            held.used = clock;
            Arc::clone(&held.file)
        })
    }

    /// Takes out of the table the files used least recently until one
    /// more fits under `cap`, and returns them so the caller closes them
    /// after letting go of the lock. A file a read is using right now
    /// stays: it is closed later, when a file is added and it is not in
    /// use. If every file is in use, the table runs over `cap` until those
    /// reads finish.
    fn make_room(&mut self, cap: usize) -> Vec<Arc<File>> {
        let mut closed = Vec::new();
        while self.files.len() >= cap {
            let idle = self
                .files
                .iter()
                .filter(|(_, held)| Arc::strong_count(&held.file) == 1)
                .min_by_key(|(_, held)| held.used)
                .map(|(token, _)| *token);
            let Some(token) = idle else {
                break;
            };
            if let Some(held) = self.files.remove(&token) {
                closed.push(held.file);
            }
        }
        closed
    }

    fn insert(&mut self, token: FileToken, file: Arc<File>) {
        self.clock += 1;
        self.files.insert(
            token,
            Held {
                file,
                used: self.clock,
            },
        );
    }
}

impl OpenFiles {
    /// A table that holds at most `cap` files open.
    ///
    /// # Panics
    ///
    /// When `cap` is zero.
    pub(crate) fn new(cap: usize) -> Self {
        assert!(cap > 0, "a table of open files holds at least one");
        Self {
            cap,
            state: Mutex::default(),
        }
    }

    /// The table every segment of the process reads through.
    pub(crate) fn process() -> &'static Arc<Self> {
        static PROCESS: OnceLock<Arc<OpenFiles>> = OnceLock::new();
        PROCESS.get_or_init(|| Arc::new(Self::new(MAX_OPEN_SEGMENT_FILES)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // The table only ever holds whole entries, so a panic while it was
        // held leaves nothing half done.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Hands `file`, just opened for the segment `token` names, to the
    /// table, closing the file used least recently if the table is full.
    pub(crate) fn insert(&self, token: FileToken, file: File) {
        let closed = {
            let mut state = self.lock();
            let closed = state.make_room(self.cap);
            state.insert(token, Arc::new(file));
            closed
        };
        drop(closed);
    }

    /// The open file of the segment `token` names, opened again by
    /// `reopen` when the table closed it.
    ///
    /// # Errors
    ///
    /// When `reopen` fails.
    pub(crate) fn get<E>(
        &self,
        token: FileToken,
        reopen: impl FnOnce() -> Result<File, E>,
    ) -> Result<Arc<File>, E> {
        if let Some(file) = self.lock().touch(token) {
            return Ok(file);
        }
        // Opened without the lock held: opening and checking reads the
        // file. Two threads that both find it closed both open it, and the
        // second keeps the first one's.
        let opened = Arc::new(reopen()?);
        let (file, closed) = {
            let mut state = self.lock();
            match state.touch(token) {
                Some(file) => (file, Vec::new()),
                None => {
                    let closed = state.make_room(self.cap);
                    state.insert(token, Arc::clone(&opened));
                    (opened, closed)
                }
            }
        };
        drop(closed);
        Ok(file)
    }

    /// Closes the file of the segment `token` names, once that segment is
    /// dropped.
    pub(crate) fn forget(&self, token: FileToken) {
        let held = self.lock().files.remove(&token);
        drop(held);
    }

    /// How many files the table holds open.
    pub(crate) fn open_count(&self) -> usize {
        self.lock().files.len()
    }
}

impl std::fmt::Debug for OpenFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenFiles")
            .field("cap", &self.cap)
            .field("open", &self.open_count())
            .finish()
    }
}

/// How many segment files this process holds open now.
#[must_use]
pub fn open_segment_files() -> usize {
    OpenFiles::process().open_count()
}
