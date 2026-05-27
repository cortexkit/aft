use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ENTRIES_PER_SESSION: usize = 50;

/// The kind of file operation recorded in session history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOp {
    Read,
    Zoom,
    Edit,
    Write,
    Delete,
    Move,
}

/// A single entry in the session file-access history.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HistoryEntry {
    pub path: PathBuf,
    pub op: FileOp,
    pub timestamp_millis: u64,
}

/// Per-session, in-memory, chronologically-ordered file-access history.
///
/// Entries are recorded by instrumenting command handlers and retained
/// for the lifetime of the bridge process. The cap is enforced per
/// session so one busy session cannot evict another's history.
///
/// This is purely in-memory — no disk persistence. The purpose is to
/// answer "what was I just working on?" within the current session.
#[derive(Debug, Clone)]
pub struct SessionHistory {
    /// session_id -> VecDeque of entries (newest first)
    sessions: std::collections::HashMap<String, VecDeque<HistoryEntry>>,
}

impl Default for SessionHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionHistory {
    pub fn new() -> Self {
        Self {
            sessions: std::collections::HashMap::new(),
        }
    }

    /// Record a file access for the given session.
    ///
    /// Entries are inserted at the front so iteration yields newest-first.
    /// When the per-session cap is exceeded, the oldest entry is dropped.
    pub fn record(&mut self, session: &str, path: PathBuf, op: FileOp) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let queue = self.sessions.entry(session.to_string()).or_default();
        // Avoid duplicate consecutive entries for the same (path, op) — if
        // the agent reads the same file twice in a row, just update the
        // timestamp of the existing head entry.
        if let Some(front) = queue.front_mut() {
            if front.path == path && front.op == op {
                front.timestamp_millis = now;
                return;
            }
        }

        queue.push_front(HistoryEntry {
            path,
            op,
            timestamp_millis: now,
        });

        while queue.len() > MAX_ENTRIES_PER_SESSION {
            queue.pop_back();
        }
    }

    /// Return recent history for a session, newest first, up to `limit`.
    pub fn recent(&self, session: &str, limit: usize) -> Vec<HistoryEntry> {
        self.sessions
            .get(session)
            .map(|queue| queue.iter().take(limit).cloned().collect())
            .unwrap_or_default()
    }

    /// Return ALL sessions that have recorded history (for diagnostics / status).
    pub fn known_sessions(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn records_and_returns_recent() {
        let mut hist = SessionHistory::new();
        hist.record("s1", Path::new("/a.ts").to_path_buf(), FileOp::Read);
        hist.record("s1", Path::new("/b.ts").to_path_buf(), FileOp::Edit);
        hist.record("s2", Path::new("/c.ts").to_path_buf(), FileOp::Read);

        let s1 = hist.recent("s1", 10);
        assert_eq!(s1.len(), 2);
        assert_eq!(s1[0].path, Path::new("/b.ts"));
        assert_eq!(s1[0].op as FileOp, FileOp::Edit);
        assert_eq!(s1[1].path, Path::new("/a.ts"));

        let s2 = hist.recent("s2", 10);
        assert_eq!(s2.len(), 1);
    }

    #[test]
    fn deduplicates_consecutive_same_op() {
        let mut hist = SessionHistory::new();
        hist.record("s1", Path::new("/a.ts").to_path_buf(), FileOp::Read);
        let ts1 = hist.recent("s1", 10)[0].timestamp_millis;

        std::thread::sleep(std::time::Duration::from_millis(2));
        hist.record("s1", Path::new("/a.ts").to_path_buf(), FileOp::Read);
        let s1 = hist.recent("s1", 10);
        assert_eq!(s1.len(), 1); // still 1 entry
        assert!(s1[0].timestamp_millis > ts1); // timestamp updated
    }

    #[test]
    fn caps_at_max_entries() {
        let mut hist = SessionHistory::new();
        for i in 0..MAX_ENTRIES_PER_SESSION + 10 {
            hist.record(
                "s1",
                Path::new(&format!("/file-{}.ts", i)).to_path_buf(),
                FileOp::Read,
            );
        }
        let entries = hist.recent("s1", usize::MAX);
        assert_eq!(entries.len(), MAX_ENTRIES_PER_SESSION);
        // Newest entry should be the last one we added
        assert_eq!(
            entries[0].path,
            Path::new(&format!("/file-{}.ts", MAX_ENTRIES_PER_SESSION + 9))
        );
    }

    #[test]
    fn empty_session_returns_empty() {
        let hist = SessionHistory::new();
        assert!(hist.recent("nonexistent", 10).is_empty());
    }

    #[test]
    fn known_sessions() {
        let mut hist = SessionHistory::new();
        hist.record("s1", Path::new("/a.ts").to_path_buf(), FileOp::Read);
        hist.record("s2", Path::new("/b.ts").to_path_buf(), FileOp::Write);
        let mut sessions = hist.known_sessions();
        sessions.sort();
        assert_eq!(sessions, vec!["s1".to_string(), "s2".to_string()]);
    }
}
