//! Keep AFT from running Apple's developer-tool shims on a Mac without them.
//!
//! On macOS, `/usr/bin/git` (and `clangd`, `sourcekit-lsp`, `python3`, `make`,
//! `xcrun`, ...) is not the tool itself but a small launcher linked against
//! `/usr/lib/libxcselect.dylib`. The launcher looks up the active developer
//! directory and runs the real tool from it. When no developer directory is
//! installed it prints "xcode-select: note: No developer tools were found,
//! requesting install" and opens the "Install Command Line Developer Tools"
//! dialog in the user's GUI session, once per invocation. So AFT must never
//! spawn a launcher on such a machine, not even with `--version`.
//!
//! The launcher's lookup order is documented in xcode-select(1): the
//! `DEVELOPER_DIR` environment variable overrides everything; otherwise the
//! system-wide selection written by `xcode-select --switch` (the
//! `/var/db/xcode_select_link` symlink) is used; with no selection the
//! defaults are `/Applications/Xcode.app/Contents/Developer` and then
//! `/Library/Developer/CommandLineTools`. `xcode-select -p` reports that same
//! state and is safe (with no tools it prints an error and exits non-zero,
//! without the dialog). Reading those directories answers the common case
//! without spawning anything, so this module does that first and asks
//! `xcode-select -p` only when no known directory holds the tools.
//!
//! The decision is made once per process. Only a launcher is ever refused: a
//! user whose PATH finds a real git first (Homebrew, MacPorts, Nix) keeps
//! every git feature even without the Xcode tools.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Test seam, read at runtime because integration tests drive the production
/// binary: `absent` or `present` replaces the developer-directory lookup.
const DEVELOPER_TOOLS_OVERRIDE_ENV: &str = "AFT_TEST_DEVELOPER_TOOLS";
/// Test seam: a PATH-style list of directories whose executables are treated
/// as Apple launchers (instead of `/usr/bin` binaries linking libxcselect).
const LAUNCHER_DIRS_OVERRIDE_ENV: &str = "AFT_TEST_XCODE_LAUNCHER_DIRS";

/// Every Apple launcher names this library in its load commands; ordinary
/// `/usr/bin` programs such as `ssh` do not.
const XCSELECT_LIBRARY_MARKER: &[u8] = b"/usr/lib/libxcselect.dylib";

/// Whether AFT may run `git` in this process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitAvailability {
    /// `git` resolves to a real binary, to Apple's launcher on a Mac that has
    /// developer tools, or to nothing (spawns then fail the ordinary way).
    Usable,
    /// `git` resolves to Apple's launcher and no developer tools exist, so
    /// running it would open the install dialog.
    MissingDeveloperTools { launcher: PathBuf },
}

/// The user-facing explanation shown by status when git features are off.
pub const MISSING_DEVELOPER_TOOLS_REASON: &str =
    "git features are off: macOS developer tools are not installed, and running \
     /usr/bin/git would open Apple's install dialog. Install them with \
     `xcode-select --install`, or put another git (for example Homebrew's) on PATH, \
     then restart AFT.";

#[derive(Clone, Debug, PartialEq, Eq)]
struct LauncherStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64, u32),
}

impl LauncherStamp {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            identity: (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
                metadata.mode(),
            ),
        }
    }
}

type LauncherCache = Arc<Mutex<HashMap<PathBuf, (LauncherStamp, bool)>>>;

/// Inputs to the launcher decision, separated from the process so tests can
/// inject a "no developer tools" machine and a fake launcher directory.
#[derive(Clone, Debug)]
pub struct Detector {
    launcher_dirs: Vec<PathBuf>,
    /// When true, a file in a launcher directory only counts as a launcher if
    /// it links libxcselect. Injected launcher directories skip this so a
    /// test can use a shell script.
    require_xcselect_marker: bool,
    developer_tools_present: bool,
    launcher_cache: LauncherCache,
    #[cfg(any(test, feature = "test-support"))]
    marker_reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Detector {
    /// Build a detector from explicit inputs; every file in `launcher_dirs`
    /// counts as a launcher.
    pub fn new(launcher_dirs: Vec<PathBuf>, developer_tools_present: bool) -> Self {
        Self {
            launcher_dirs: canonical_dirs(launcher_dirs),
            require_xcselect_marker: false,
            developer_tools_present,
            launcher_cache: Default::default(),
            #[cfg(any(test, feature = "test-support"))]
            marker_reads: Default::default(),
        }
    }

    fn from_process() -> Self {
        let override_dirs = std::env::var_os(LAUNCHER_DIRS_OVERRIDE_ENV)
            .filter(|value| !value.is_empty())
            .map(|value| std::env::split_paths(&value).collect::<Vec<_>>());
        let override_tools = std::env::var(DEVELOPER_TOOLS_OVERRIDE_ENV)
            .ok()
            .and_then(|value| match value.as_str() {
                "absent" => Some(false),
                "present" => Some(true),
                _ => None,
            });

        let (launcher_dirs, require_xcselect_marker) = match override_dirs {
            Some(dirs) => (canonical_dirs(dirs), false),
            None if cfg!(target_os = "macos") => (vec![PathBuf::from("/usr/bin")], true),
            None => (Vec::new(), false),
        };
        // Only look at the developer directories when a launcher could matter;
        // on other platforms the answer is irrelevant.
        let developer_tools_present = override_tools.unwrap_or_else(|| {
            launcher_dirs.is_empty()
                || macos_developer_tools_present(DeveloperDirProbe::system())
                || xcode_select_reports_developer_dir()
        });
        Self {
            launcher_dirs,
            require_xcselect_marker,
            developer_tools_present,
            launcher_cache: Default::default(),
            #[cfg(any(test, feature = "test-support"))]
            marker_reads: Default::default(),
        }
    }

    pub fn developer_tools_present(&self) -> bool {
        self.developer_tools_present
    }

    /// True when `path` is an Apple launcher that would open the install
    /// dialog if run. Reuses marker reads while file metadata is unchanged;
    /// never executes the candidate.
    pub fn is_unusable_launcher(&self, path: &Path) -> bool {
        self.is_unusable_launcher_with_reader(path, |path| std::fs::read(path))
    }

    fn is_unusable_launcher_with_reader(
        &self,
        path: &Path,
        read: impl FnOnce(&Path) -> std::io::Result<Vec<u8>>,
    ) -> bool {
        if self.developer_tools_present || self.launcher_dirs.is_empty() {
            return false;
        }
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let in_launcher_dir = canonical
            .parent()
            .is_some_and(|parent| self.launcher_dirs.iter().any(|dir| dir == parent));
        if !in_launcher_dir {
            return false;
        }
        if !self.require_xcselect_marker {
            return true;
        }
        let Ok(metadata) = std::fs::metadata(&canonical) else {
            return true;
        };
        let stamp = LauncherStamp::from_metadata(&metadata);
        let mut cache = self
            .launcher_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some((cached_stamp, decision)) = cache.get(&canonical) {
            if cached_stamp == &stamp {
                return *decision;
            }
        }
        #[cfg(any(test, feature = "test-support"))]
        self.marker_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let bytes = match read(&canonical) {
            Ok(bytes) => bytes,
            // Refuse an unreadable candidate because it may open the install
            // dialog, but retry transient errors instead of caching a guess.
            Err(_) => return true,
        };
        let decision = contains_subslice(&bytes, XCSELECT_LIBRARY_MARKER);
        cache.insert(canonical, (stamp, decision));
        decision
    }

    /// Decide whether `git` found on `path_var` may run.
    pub fn git_availability(&self, path_var: &OsStr) -> GitAvailability {
        if self.developer_tools_present || self.launcher_dirs.is_empty() {
            return GitAvailability::Usable;
        }
        match first_on_path(path_var, "git") {
            Some(git) if self.is_unusable_launcher(&git) => {
                GitAvailability::MissingDeveloperTools { launcher: git }
            }
            _ => GitAvailability::Usable,
        }
    }
}

fn canonical_dirs(dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    dirs.into_iter()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| std::fs::canonicalize(&dir).unwrap_or(dir))
        .collect()
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// First executable `binary` on a PATH-style list, the one a spawn would run.
fn first_on_path(path_var: &OsStr, binary: &str) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| crate::tool_path::probe_tool_in_dir(&dir, binary))
}

/// Where the launcher looks for a developer directory, in its own order.
#[derive(Clone, Debug)]
pub struct DeveloperDirProbe {
    pub developer_dir_env: Option<PathBuf>,
    pub selection_link: PathBuf,
    pub defaults: Vec<PathBuf>,
}

impl DeveloperDirProbe {
    fn system() -> Self {
        Self {
            developer_dir_env: crate::environment::non_empty_os_var("DEVELOPER_DIR")
                .map(PathBuf::from),
            selection_link: PathBuf::from("/var/db/xcode_select_link"),
            defaults: vec![
                PathBuf::from("/Applications/Xcode.app/Contents/Developer"),
                PathBuf::from("/Library/Developer/CommandLineTools"),
            ],
        }
    }
}

/// A developer directory counts only if it holds the tools (`usr/bin`); an
/// empty leftover directory would still leave the launcher prompting.
fn is_developer_dir(dir: &Path) -> bool {
    dir.join("usr").join("bin").is_dir()
}

/// Follow the launcher's lookup order without running anything.
pub fn macos_developer_tools_present(probe: DeveloperDirProbe) -> bool {
    if let Some(dir) = probe.developer_dir_env {
        // DEVELOPER_DIR replaces the lookup entirely, so a bad value is not
        // rescued by an installed default.
        return is_developer_dir(&dir);
    }
    if let Ok(selected) = std::fs::canonicalize(&probe.selection_link) {
        if is_developer_dir(&selected) {
            return true;
        }
    }
    probe.defaults.iter().any(|dir| is_developer_dir(dir))
}

/// Second opinion for a Mac where none of the known directories hold the
/// tools: Xcode may live under another name (say `Xcode-beta.app`) that the
/// launcher still finds. `xcode-select -p` answers from the same lookup and,
/// unlike the launchers, only prints an error and exits non-zero when nothing
/// is installed; it never opens the install dialog. It runs at most once per
/// process, and only on a Mac that looks like it has no tools.
fn xcode_select_reports_developer_dir() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    std::process::Command::new("/usr/bin/xcode-select")
        .arg("-p")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && is_developer_dir(Path::new(String::from_utf8_lossy(&output.stdout).trim()))
        })
}

fn process_detector() -> &'static Detector {
    static DETECTOR: OnceLock<Detector> = OnceLock::new();
    DETECTOR.get_or_init(Detector::from_process)
}

/// The once-per-process git decision. Logs once when git is turned off.
pub fn git_availability() -> &'static GitAvailability {
    static GIT: OnceLock<GitAvailability> = OnceLock::new();
    GIT.get_or_init(|| {
        let availability =
            process_detector().git_availability(crate::effective_path::effective_path());
        if let GitAvailability::MissingDeveloperTools { launcher } = &availability {
            log::warn!(
                "git disabled for this process: {} is Apple's developer-tools launcher and no \
                 developer tools are installed; running it would open the install dialog. \
                 Git-backed features use their non-git fallbacks.",
                launcher.display()
            );
        }
        availability
    })
}

/// Whether engine code may spawn `git`. A disabled tool cannot determine whether
/// a directory is a repository or which index data belongs to it. Preserve that
/// uncertainty rather than using it to choose another identity or delete data.
pub fn git_usable() -> bool {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(usable) = TEST_GIT_USABLE.with(|value| value.get()) {
        return usable;
    }
    matches!(git_availability(), GitAvailability::Usable)
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static TEST_GIT_USABLE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn with_git_usable_for_test<T>(usable: bool, run: impl FnOnce() -> T) -> T {
    struct Reset(Option<bool>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_GIT_USABLE.with(|value| value.set(self.0));
        }
    }
    let _reset = Reset(TEST_GIT_USABLE.with(|value| value.replace(Some(usable))));
    run()
}

/// Status payload describing whether git features are on.
pub fn git_status_json() -> serde_json::Value {
    match git_availability() {
        GitAvailability::Usable => serde_json::json!({ "available": true }),
        GitAvailability::MissingDeveloperTools { launcher } => serde_json::json!({
            "available": false,
            "reason": "macos_developer_tools_missing",
            "message": MISSING_DEVELOPER_TOOLS_REASON,
            "launcher": launcher.display().to_string(),
        }),
    }
}

/// Whether a resolved tool path (LSP server, formatter) is an Apple launcher
/// that must not run on this machine. Callers treat it as not installed.
pub fn is_unusable_launcher(path: &Path) -> bool {
    process_detector().is_unusable_launcher(path)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn launcher_read_error_is_retried_without_metadata_changes() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("git");
        std::fs::write(&tool, b"a real tool without the launcher marker").unwrap();
        let mut detector = Detector::new(vec![dir.path().to_path_buf()], false);
        detector.require_xcselect_marker = true;
        let before = LauncherStamp::from_metadata(&std::fs::metadata(&tool).unwrap());

        assert!(detector.is_unusable_launcher_with_reader(&tool, |_| {
            Err(std::io::Error::from_raw_os_error(libc::EMFILE))
        }));
        assert_eq!(
            before,
            LauncherStamp::from_metadata(&std::fs::metadata(&tool).unwrap())
        );
        assert!(!detector.is_unusable_launcher(&tool));
        assert_eq!(
            detector
                .marker_reads
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
        assert!(!detector.is_unusable_launcher(&tool));
        assert_eq!(
            detector
                .marker_reads
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
    }

    #[test]
    fn launcher_marker_is_memoized_and_invalidated_by_file_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let launcher = dir.path().join("git");
        std::fs::write(&launcher, XCSELECT_LIBRARY_MARKER).unwrap();
        let mut detector = Detector::new(vec![dir.path().to_path_buf()], false);
        detector.require_xcselect_marker = true;
        assert!(detector.is_unusable_launcher(&launcher));
        assert!(detector.clone().is_unusable_launcher(&launcher));
        assert_eq!(
            detector
                .marker_reads
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );

        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, vec![b'x'; XCSELECT_LIBRARY_MARKER.len()]).unwrap();
        // Equal size and mtime must not hide a replacement inode from the cache.
        let original_time = std::fs::metadata(&launcher).unwrap().modified().unwrap();
        std::fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_modified(original_time)
            .unwrap();
        std::fs::remove_file(&launcher).unwrap();
        std::fs::rename(&replacement, &launcher).unwrap();
        assert!(!detector.is_unusable_launcher(&launcher));
        assert!(!detector.is_unusable_launcher(&launcher));
        assert_eq!(
            detector
                .marker_reads
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
    }

    fn executable(path: &Path, body: &str) {
        crate::executable_cache::write_executable(path, body.as_bytes()).unwrap();
    }

    #[test]
    fn launcher_git_without_developer_tools_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let launchers = temp.path().join("usr-bin");
        std::fs::create_dir_all(&launchers).unwrap();
        executable(&launchers.join("git"), "#!/bin/sh\nexit 1\n");

        let detector = Detector::new(vec![launchers.clone()], false);
        assert_eq!(
            detector.git_availability(launchers.as_os_str()),
            GitAvailability::MissingDeveloperTools {
                launcher: launchers.join("git")
            }
        );
        // The same launcher is fine once developer tools exist.
        let with_tools = Detector::new(vec![launchers.clone()], true);
        assert_eq!(
            with_tools.git_availability(launchers.as_os_str()),
            GitAvailability::Usable
        );
    }

    #[test]
    fn real_git_earlier_on_path_wins_over_the_launcher() {
        let temp = tempfile::tempdir().unwrap();
        let launchers = temp.path().join("usr-bin");
        let homebrew = temp.path().join("homebrew-bin");
        std::fs::create_dir_all(&launchers).unwrap();
        std::fs::create_dir_all(&homebrew).unwrap();
        executable(&launchers.join("git"), "#!/bin/sh\nexit 1\n");
        executable(&homebrew.join("git"), "#!/bin/sh\nexit 0\n");

        let detector = Detector::new(vec![launchers.clone()], false);
        let path = std::env::join_paths([&homebrew, &launchers]).unwrap();
        assert_eq!(detector.git_availability(&path), GitAvailability::Usable);
        // Order matters: the launcher first is still refused.
        let reversed = std::env::join_paths([&launchers, &homebrew]).unwrap();
        assert!(matches!(
            detector.git_availability(&reversed),
            GitAvailability::MissingDeveloperTools { .. }
        ));
    }

    #[test]
    fn marker_check_keeps_ordinary_binaries_in_the_launcher_dir() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("usr-bin");
        std::fs::create_dir_all(&dir).unwrap();
        executable(&dir.join("ssh"), "plain program\n");
        executable(&dir.join("clangd"), "...\0/usr/lib/libxcselect.dylib\0...");
        let mut detector = Detector::new(vec![dir.clone()], false);
        detector.require_xcselect_marker = true;
        assert!(!detector.is_unusable_launcher(&dir.join("ssh")));
        assert!(detector.is_unusable_launcher(&dir.join("clangd")));
    }

    #[test]
    fn developer_dir_lookup_follows_the_launcher_order() {
        let temp = tempfile::tempdir().unwrap();
        let installed = temp.path().join("CommandLineTools");
        std::fs::create_dir_all(installed.join("usr/bin")).unwrap();
        let empty = temp.path().join("Leftover");
        std::fs::create_dir_all(&empty).unwrap();
        let missing_link = temp.path().join("no-link");

        let probe = |env: Option<&Path>, link: &Path, defaults: Vec<PathBuf>| DeveloperDirProbe {
            developer_dir_env: env.map(Path::to_path_buf),
            selection_link: link.to_path_buf(),
            defaults,
        };
        assert!(!macos_developer_tools_present(probe(
            None,
            &missing_link,
            vec![empty.clone(), temp.path().join("Xcode.app")]
        )));
        assert!(macos_developer_tools_present(probe(
            None,
            &missing_link,
            vec![installed.clone()]
        )));
        let link = temp.path().join("xcode_select_link");
        std::os::unix::fs::symlink(&installed, &link).unwrap();
        assert!(macos_developer_tools_present(probe(None, &link, vec![])));
        // DEVELOPER_DIR replaces the lookup, even when a default is installed.
        assert!(!macos_developer_tools_present(probe(
            Some(&empty),
            &link,
            vec![installed.clone()]
        )));
        assert!(macos_developer_tools_present(probe(
            Some(&installed),
            &missing_link,
            vec![]
        )));
    }
}
