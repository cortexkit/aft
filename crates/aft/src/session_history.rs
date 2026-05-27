use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ENTRIES_PER_SESSION: usize = 50;
const MAX_SESSIONS: usize = 100;

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
    /// LRU order of session IDs — oldest at front, most recently used at back.
    lru: VecDeque<String>,
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
            lru: VecDeque::new(),
        }
    }

    fn touch_session(&mut self, session: &str) {
        // Move session to back (most recently used) in the LRU list.
        if let Some(pos) = self.lru.iter().position(|s| s == session) {
            self.lru.remove(pos);
        }
        self.lru.push_back(session.to_string());
    }

    fn evict_oldest_session(&mut self) {
        while self.sessions.len() > MAX_SESSIONS {
            if let Some(oldest) = self.lru.pop_front() {
                self.sessions.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Record a file access for the given session.
    ///
    /// Entries are inserted at the front so iteration yields newest-first.
    /// When the per-session cap is exceeded, the oldest entry is dropped.
    /// When the total session count exceeds MAX_SESSIONS, the least recently
    /// used session is evicted entirely.
    pub fn record(&mut self, session: &str, path: PathBuf, op: FileOp) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let is_new = !self.sessions.contains_key(session);

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

        // Only touch LRU on new-session creation, not on every record call.
        // This prevents continuous reordering from keeping stale sessions alive.
        if is_new {
            self.touch_session(session);
            self.evict_oldest_session();
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

    /// Close a session and free its memory. Returns the removed entries.
    pub fn close_session(&mut self, session: &str) -> Vec<HistoryEntry> {
        let entries = self.sessions.remove(session).map_or_else(Vec::new, |q| q.into());
        if let Some(pos) = self.lru.iter().position(|s| s == session) {
            self.lru.remove(pos);
        }
        entries
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
