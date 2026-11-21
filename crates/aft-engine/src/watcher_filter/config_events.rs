use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long a burst of file events must be quiet before the reload runs.
/// Editors save by writing a temporary sibling and renaming it over the file,
/// which arrives as several events.
pub const CONFIG_RELOAD_DEBOUNCE: Duration = Duration::from_millis(150);

fn process_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn now_ms() -> u64 {
    u64::try_from(process_epoch().elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// A pending "re-read the config files" request for one root, with a trailing
/// debounce: every new request pushes the due time out again.
#[derive(Debug, Default)]
pub struct ConfigReloadSignal {
    /// Milliseconds since the process epoch at which the reload is due; zero
    /// when no reload is pending.
    pub(crate) due_at_ms: AtomicU64,
}

impl ConfigReloadSignal {
    /// Ask for a reload once the debounce window has passed quietly.
    pub fn request(&self) {
        self.request_after(CONFIG_RELOAD_DEBOUNCE);
    }

    fn request_after(&self, delay: Duration) {
        let due = now_ms()
            .saturating_add(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX))
            .max(1);
        self.due_at_ms.store(due, Ordering::Release);
    }

    /// Ask for a reload that is due at once (used when a root reattaches after
    /// idle eviction: the content check makes an unneeded reload a no-op).
    pub fn request_now(&self) {
        self.request_after(Duration::ZERO);
    }

    pub fn is_pending(&self) -> bool {
        self.due_at_ms.load(Ordering::Acquire) != 0
    }

    pub fn is_due(&self) -> bool {
        let due = self.due_at_ms.load(Ordering::Acquire);
        due != 0 && due <= now_ms()
    }

    /// Clear a due request and report whether there was one. A request that
    /// arrives while the reload runs sets a new due time and is not lost.
    pub fn take_if_due(&self) -> bool {
        let due = self.due_at_ms.load(Ordering::Acquire);
        if due == 0 || due > now_ms() {
            return false;
        }
        self.due_at_ms
            .compare_exchange(due, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// Whether `path` names a shared or legacy project config, or an editor's
/// temporary sibling. `root` is the configured canonical directory; the
/// event's containing root is canonicalized if needed, while filenames stay literal.
pub fn is_project_config_event_path(root: &Path, path: &Path) -> bool {
    if !is_config_file_name(path) {
        return false;
    }
    let Some(dir) = path.parent() else {
        return false;
    };
    if !matches!(
        dir.file_name().and_then(|name| name.to_str()),
        Some(".cortexkit" | ".opencode" | ".pi")
    ) {
        return false;
    }
    let Some(dir_root) = dir.parent() else {
        return false;
    };
    // Raw watcher events may spell the root differently from the configured
    // canonical root (macOS reports `/private/var` for `/var`); resolve only
    // the root directory, never the config file name itself.
    dir_root == root || std::fs::canonicalize(dir_root).is_ok_and(|resolved| resolved == root)
}

pub(crate) fn is_config_file_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("aft.json") || name.starts_with(".aft.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_debounces_until_quiet() {
        let signal = ConfigReloadSignal::default();
        assert!(!signal.take_if_due());
        signal.request();
        assert!(signal.is_pending());
        assert!(!signal.take_if_due(), "not due inside the debounce window");
        std::thread::sleep(CONFIG_RELOAD_DEBOUNCE + Duration::from_millis(50));
        assert!(signal.take_if_due());
        assert!(!signal.is_pending());
    }

    #[test]
    fn project_watcher_filter_recognises_the_config_file() {
        let root = Path::new("/r");
        assert!(is_project_config_event_path(
            root,
            Path::new("/r/.cortexkit/aft.jsonc")
        ));
        assert!(is_project_config_event_path(
            root,
            Path::new("/r/.cortexkit/aft.jsonc.tmp.123")
        ));
        assert!(!is_project_config_event_path(
            root,
            Path::new("/r/.cortexkit/other.json")
        ));
        assert!(!is_project_config_event_path(
            root,
            Path::new("/r/aft.jsonc")
        ));
        for directory in [".opencode", ".pi"] {
            for filename in [
                "aft.jsonc",
                "aft.json",
                "aft.json.tmp.123",
                ".aft.jsonc.tmp.123",
            ] {
                assert!(is_project_config_event_path(
                    root,
                    &root.join(directory).join(filename)
                ));
            }
            assert!(!is_project_config_event_path(
                root,
                &root.join(directory).join("other.json")
            ));
        }
    }
}
