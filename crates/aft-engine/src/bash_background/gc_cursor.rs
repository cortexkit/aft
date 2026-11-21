//! A resumable directory walk: the budget applies to iterator advancement,
//! including invalid entries and session directories, not just deletions.
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, ReadDir};
use std::path::{Path, PathBuf};

use super::persistence::validate_task_id;

pub(crate) const GC_ENTRY_BUDGET: usize = 128;

pub(crate) struct Batch {
    pub session: PathBuf,
    pub ids: Vec<String>,
    pub invalid: Vec<OsString>,
    pub names: Vec<OsString>,
}

#[derive(Default)]
pub(crate) struct BatchResult {
    pub batches: Vec<Batch>,
    pub unreadable_sessions: usize,
    pub unreadable_session_paths: Vec<PathBuf>,
    pub directory_read_errors: usize,
    pub root_directory_read_error: bool,
    /// The first walk error in this lap, retained when the cursor resumes.
    pub first_error: Option<String>,
}

pub(crate) struct Cursor {
    root: PathBuf,
    sessions: Entries,
    tasks: Option<(PathBuf, Entries)>,
    first_error: Option<String>,
    pub finished: bool,
}

const MAX_ERROR_MESSAGE_BYTES: usize = 512;

fn bounded_error_message(message: String) -> String {
    if message.len() <= MAX_ERROR_MESSAGE_BYTES {
        return message;
    }
    let mut end = MAX_ERROR_MESSAGE_BYTES - '…'.len_utf8();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &message[..end])
}

impl Cursor {
    pub fn new(root: &Path) -> std::io::Result<Self> {
        Ok(Self {
            root: root.to_path_buf(),
            sessions: Entries::open(root)?,
            tasks: None,
            first_error: None,
            finished: false,
        })
    }

    pub fn next_batch(&mut self) -> BatchResult {
        let mut batches: Vec<Batch> = Vec::new();
        let mut unreadable_sessions = 0usize;
        let mut unreadable_session_paths = Vec::new();
        let mut directory_read_errors = 0usize;
        let mut root_directory_read_error = false;
        let mut ids = BTreeSet::new();
        for _ in 0..GC_ENTRY_BUDGET {
            if let Some((session, entries)) = &mut self.tasks {
                if batches.last().is_none_or(|batch| &batch.session != session) {
                    ids.clear();
                    batches.push(Batch {
                        session: session.clone(),
                        ids: Vec::new(),
                        invalid: Vec::new(),
                        names: Vec::new(),
                    });
                }
                let Some(entry) = entries.next() else {
                    self.tasks = None;
                    continue;
                };
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        directory_read_errors = directory_read_errors.saturating_add(1);
                        unreadable_session_paths.push(session.clone());
                        if self.first_error.is_none() {
                            self.first_error = Some(bounded_error_message(format!(
                                "failed to read session {}: {error}",
                                session.display()
                            )));
                        }
                        continue;
                    }
                };
                let name = entry.file_name();
                let batch = batches.last_mut().expect("session batch");
                batch.names.push(name.clone());
                let Some(text) = name.to_str() else {
                    batch.invalid.push(name);
                    continue;
                };
                let id = text.split_once('.').map_or(text, |(id, _)| id);
                if validate_task_id(id).is_ok() {
                    if ids.insert(id.to_string()) {
                        batch.ids.push(id.to_string());
                    }
                } else if text.starts_with("bash-") {
                    batch.invalid.push(name);
                }
            } else {
                let Some(entry) = self.sessions.next() else {
                    self.finished = true;
                    break;
                };
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        directory_read_errors = directory_read_errors.saturating_add(1);
                        root_directory_read_error = true;
                        if self.first_error.is_none() {
                            self.first_error = Some(bounded_error_message(format!(
                                "failed to read {}: {error}",
                                self.root.display()
                            )));
                        }
                        continue;
                    }
                };
                let session = entry.path();
                match entry.file_type() {
                    Ok(kind) if kind.is_dir() => {}
                    Ok(_) => continue,
                    Err(error) => {
                        directory_read_errors = directory_read_errors.saturating_add(1);
                        unreadable_session_paths.push(session.clone());
                        if self.first_error.is_none() {
                            self.first_error = Some(bounded_error_message(format!(
                                "failed to inspect session entry {}: {error}",
                                session.display()
                            )));
                        }
                        continue;
                    }
                }
                match Entries::open(&session) {
                    Ok(entries) => self.tasks = Some((session, entries)),
                    Err(error) => {
                        unreadable_sessions = unreadable_sessions.saturating_add(1);
                        unreadable_session_paths.push(session.clone());
                        if self.first_error.is_none() {
                            self.first_error = Some(bounded_error_message(format!(
                                "failed to read session {}: {error}",
                                session.display()
                            )));
                        }
                    }
                }
            }
        }
        BatchResult {
            batches,
            unreadable_sessions,
            unreadable_session_paths,
            directory_read_errors,
            root_directory_read_error,
            first_error: self.first_error.clone(),
        }
    }
}

/// Count advancement at the actual iterator boundary, so eagerly collecting
/// names before applying a task budget also trips the work-count regression.
struct Entries(ReadDir);

impl Entries {
    fn open(path: &Path) -> std::io::Result<Self> {
        #[cfg(test)]
        super::persistence::work_counts::record_open();
        fs::read_dir(path).map(Self)
    }
}

impl Iterator for Entries {
    type Item = std::io::Result<fs::DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        #[cfg(test)]
        super::persistence::work_counts::record_gc_entry();
        self.0.next()
    }
}
