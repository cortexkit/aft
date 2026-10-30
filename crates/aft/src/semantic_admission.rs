//! Which project roots may build or refresh a semantic index.
//!
//! Embedding a repository is the most expensive thing AFT does: a large
//! checkout costs many hours of local embedding load. AFT therefore builds and
//! refreshes semantic indexes only for repositories a person actually works
//! in. Every other root keeps exact-text (trigram) and identifier search, and
//! may still read a semantic index someone already saved on disk, but never
//! starts or refreshes one.
//!
//! A root is *opened* when one of these holds:
//!
//! 1. An interactive harness session (OpenCode or Pi, the AFT plugins' routes)
//!    has the root bound right now.
//! 2. The root belongs to the same Git repository (the same Git common
//!    directory) as a root an interactive session has bound: a delegated
//!    worker's linked worktree of a repository the user has open.
//! 3. The root lies inside a user-tier `index.roots` entry that selects the
//!    `semantic` index (a standing root the user asked AFT to keep indexed).
//!
//! Everything else is not opened: helper routes such as sidekick or reader
//! runs and the runner preflight (harness `runner`), MCP binds, federated
//! binds, and a cross-project `aft_search` with `path`.
//!
//! [`admission`] is the single predicate every semantic start, refresh and
//! notice consults. The registry it reads is process-wide because a worker
//! worktree's answer depends on what other roots (other actors) have open.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use parking_lot::Mutex;

use crate::config::{IndexKind, IndexRootConfig};
use crate::harness::Harness;

/// Text stored as the semantic status of a root that is not opened and has no
/// saved semantic index to read. Surfaces recognise it with
/// [`is_not_opened_status`] and report `off (not opened)`.
pub const NOT_OPENED_STATUS: &str =
    "semantic indexing is off: this project is not open in any interactive session";

/// The label status surfaces show for a root that is not opened.
pub const NOT_OPENED_LABEL: &str = "off (not opened)";

/// Why a root may build a semantic index, or that it may not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticAdmission {
    /// An interactive session has this root bound.
    InteractiveSession,
    /// This root is a checkout of a repository an interactive session has open.
    OpenedRepository,
    /// A user-tier `index.roots` entry selecting `semantic` covers this root.
    StandingRoot,
    /// None of the above: read a saved index if one exists, never build one.
    NotOpened,
}

impl SemanticAdmission {
    pub fn admits(self) -> bool {
        self != Self::NotOpened
    }
}

/// Whether a bind under `harness` opens its root. Only the interactive AFT
/// plugins (OpenCode and Pi) do; every route a program opens on its own
/// behalf (`runner`, `mcp:*`, `fed:*`, internal helpers) does not.
pub fn harness_opens_root(harness: &str) -> bool {
    harness
        .parse::<Harness>()
        .is_ok_and(|harness| opens_root(&harness))
}

/// [`harness_opens_root`] for an already parsed harness.
pub fn opens_root(harness: &Harness) -> bool {
    matches!(harness, Harness::Opencode | Harness::Pi)
}

#[derive(Debug, Clone)]
struct OpenedRoot {
    root: PathBuf,
    repository: Option<PathBuf>,
}

#[derive(Default)]
struct Registry {
    /// Keyed by (session, canonical root): one interactive session may bind
    /// several roots, and several sessions may bind one root.
    opened: HashMap<(String, PathBuf), OpenedRoot>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));

/// Per-session record of the roots whose "semantic search is off" notice the
/// session has already been shown, so it is shown once per session and root.
static NOTICES_SHOWN: LazyLock<Mutex<HashSet<(String, PathBuf)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn canonical(root: &Path) -> PathBuf {
    crate::path_identity::ProjectRootId::from_path(root)
        .map(crate::path_identity::ProjectRootId::into_path_buf)
        .unwrap_or_else(|_| root.to_path_buf())
}

/// Record that interactive `session` has `root` bound.
pub fn note_opened(session: &str, root: &Path) {
    let root = canonical(root);
    let repository = repository_identity(&root);
    REGISTRY.lock().opened.insert(
        (session.to_string(), root.clone()),
        OpenedRoot { root, repository },
    );
}

/// Record that `session` no longer has `root` bound. A root another session
/// still has open stays opened.
pub fn note_closed(session: &str, root: &Path) {
    let root = canonical(root);
    REGISTRY.lock().opened.remove(&(session.to_string(), root));
}

/// The one predicate deciding whether `root` may build or refresh a semantic
/// index. `standing_roots` is the user-tier `index.roots` list from the
/// resolved configuration.
pub fn admission(root: &Path, standing_roots: &[IndexRootConfig]) -> SemanticAdmission {
    admission_with_repository_identity(
        &REGISTRY,
        &canonical(root),
        standing_roots,
        repository_identity,
    )
}

fn admission_with_repository_identity(
    roots: &Mutex<Registry>,
    root: &Path,
    standing_roots: &[IndexRootConfig],
    resolve_repository: impl FnOnce(&Path) -> Option<PathBuf>,
) -> SemanticAdmission {
    let needs_repository = {
        let registry = roots.lock();
        if registry.opened.values().any(|opened| opened.root == root) {
            return SemanticAdmission::InteractiveSession;
        }
        !registry.opened.is_empty()
    };
    // Git identity discovery can read and canonicalize several filesystem
    // paths. Other sessions must be able to bind or close roots while it runs.
    let repository = needs_repository.then(|| resolve_repository(root)).flatten();
    {
        let registry = roots.lock();
        if registry.opened.values().any(|opened| opened.root == root) {
            return SemanticAdmission::InteractiveSession;
        }
        if let Some(repository) = repository {
            if registry
                .opened
                .values()
                .any(|opened| opened.repository.as_ref() == Some(&repository))
            {
                return SemanticAdmission::OpenedRepository;
            }
        }
    }
    if standing_root_covers(root, standing_roots) {
        return SemanticAdmission::StandingRoot;
    }
    SemanticAdmission::NotOpened
}

fn standing_root_covers(root: &Path, standing_roots: &[IndexRootConfig]) -> bool {
    if standing_roots.is_empty() {
        return false;
    }
    let home = crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"))
        .map(PathBuf::from);
    standing_roots
        .iter()
        .filter(|entry| entry.indexes.contains(&IndexKind::Semantic))
        .filter_map(|entry| {
            crate::config::expand_index_root_path(&entry.path, home.as_deref()).ok()
        })
        .any(|path| root.starts_with(canonical(&path)))
}

/// The admission of one root, re-evaluated each time a long-lived semantic
/// worker is about to embed, so a root that stops being opened stops
/// refreshing without its worker being torn down.
#[derive(Debug, Clone)]
pub struct LiveAdmission {
    /// `None` admits unconditionally: a worker a test drives directly.
    subject: Option<(PathBuf, std::sync::Arc<Vec<IndexRootConfig>>)>,
}

impl LiveAdmission {
    pub fn for_root(root: &Path, standing_roots: &[IndexRootConfig]) -> Self {
        Self {
            subject: Some((
                root.to_path_buf(),
                std::sync::Arc::new(standing_roots.to_vec()),
            )),
        }
    }

    #[cfg(test)]
    pub fn always() -> Self {
        Self { subject: None }
    }

    pub fn admits(&self) -> bool {
        match &self.subject {
            Some((root, standing_roots)) => admission(root, standing_roots).admits(),
            None => true,
        }
    }
}

/// The Git common directory of the repository containing `root`, read from
/// the `.git` marker without running git. A linked worktree's `.git` file
/// names its private Git directory, whose `commondir` file names the shared
/// one; a main checkout's `.git` directory is the common directory itself.
/// `None` for a root outside any Git repository.
fn repository_identity(root: &Path) -> Option<PathBuf> {
    let mut dir = Some(root);
    while let Some(current) = dir {
        let marker = current.join(".git");
        if let Ok(metadata) = std::fs::metadata(&marker) {
            let git_dir = if metadata.is_dir() {
                marker
            } else {
                let pointer = std::fs::read_to_string(&marker).ok()?;
                let pointer = pointer.trim().strip_prefix("gitdir:")?.trim();
                let pointer = PathBuf::from(pointer);
                if pointer.is_absolute() {
                    pointer
                } else {
                    current.join(pointer)
                }
            };
            let common = match std::fs::read_to_string(git_dir.join("commondir")) {
                Ok(text) => {
                    let text = PathBuf::from(text.trim());
                    if text.is_absolute() {
                        text
                    } else {
                        git_dir.join(text)
                    }
                }
                Err(_) => git_dir,
            };
            return Some(std::fs::canonicalize(&common).unwrap_or(common));
        }
        dir = current.parent();
    }
    None
}

/// Whether a semantic status message is the not-opened marker.
pub fn is_not_opened_status(message: &str) -> bool {
    message == NOT_OPENED_STATUS
}

/// The one-line notice an `aft_search` reply carries the first time `session`
/// searches `root` while semantic search is off for it, or `None` when the
/// session has already been told.
pub fn take_notice(session: &str, root: &Path) -> Option<String> {
    let root = canonical(root);
    if !NOTICES_SHOWN
        .lock()
        .insert((session.to_string(), root.clone()))
    {
        return None;
    }
    let repo = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());
    Some(format!(
        "semantic search is off for {repo}: it is not open in any session. Ask the user before enabling it; they can add {} to index.roots in ~/.config/cortexkit/aft.jsonc.",
        root.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit10_repository_resolution_does_not_hold_registry() {
        let opened = tempfile::tempdir().unwrap();
        let subject = tempfile::tempdir().unwrap();
        let roots = Mutex::new(Registry::default());
        let root = canonical(opened.path());
        roots.lock().opened.insert(
            ("test".to_string(), root.clone()),
            OpenedRoot {
                root,
                repository: None,
            },
        );
        let result =
            admission_with_repository_identity(&roots, &canonical(subject.path()), &[], |_| {
                assert!(
                    roots.try_lock().is_some(),
                    "repository I/O held the registry lock"
                );
                None
            });
        assert_eq!(result, SemanticAdmission::NotOpened);
    }

    fn git_repo(path: &Path) {
        std::fs::create_dir_all(path.join(".git")).unwrap();
    }

    fn linked_worktree(main: &Path, worktree: &Path, name: &str) {
        let private = main.join(".git").join("worktrees").join(name);
        std::fs::create_dir_all(&private).unwrap();
        std::fs::write(private.join("commondir"), "../..\n").unwrap();
        std::fs::create_dir_all(worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", private.display()),
        )
        .unwrap();
    }

    #[test]
    fn only_interactive_harnesses_open_roots() {
        assert!(harness_opens_root("opencode"));
        assert!(harness_opens_root("pi"));
        assert!(!harness_opens_root("runner"));
        assert!(!harness_opens_root("mcp:claude-code"));
        assert!(!harness_opens_root("aft-gh-shim"));
        assert!(!harness_opens_root(&format!("fed:{}", "a".repeat(32))));
    }

    #[test]
    fn a_root_is_opened_only_while_an_interactive_session_binds_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        git_repo(&root);
        assert_eq!(admission(&root, &[]), SemanticAdmission::NotOpened);
        note_opened("ses-open", &root);
        assert_eq!(admission(&root, &[]), SemanticAdmission::InteractiveSession);
        note_closed("ses-open", &root);
        assert_eq!(admission(&root, &[]), SemanticAdmission::NotOpened);
    }

    #[test]
    fn a_linked_worktree_of_an_opened_repository_is_admitted() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        git_repo(&main);
        let worktree = dir.path().join("worktree");
        linked_worktree(&main, &worktree, "task");
        let unrelated = dir.path().join("unrelated");
        git_repo(&unrelated);

        assert_eq!(admission(&worktree, &[]), SemanticAdmission::NotOpened);
        note_opened("ses-main", &main);
        assert_eq!(
            admission(&worktree, &[]),
            SemanticAdmission::OpenedRepository
        );
        assert_eq!(admission(&unrelated, &[]), SemanticAdmission::NotOpened);
        note_closed("ses-main", &main);
        assert_eq!(admission(&worktree, &[]), SemanticAdmission::NotOpened);
    }

    #[test]
    fn a_standing_root_selecting_semantic_admits_roots_inside_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("clones").join("project");
        git_repo(&root);
        let entry = |indexes: Vec<IndexKind>| IndexRootConfig {
            path: dir.path().join("clones").display().to_string(),
            indexes,
        };
        assert_eq!(
            admission(&root, &[entry(vec![IndexKind::Search])]),
            SemanticAdmission::NotOpened
        );
        assert_eq!(
            admission(
                &root,
                &[entry(vec![IndexKind::Search, IndexKind::Semantic])]
            ),
            SemanticAdmission::StandingRoot
        );
    }

    #[test]
    fn the_notice_is_taken_once_per_session_and_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("openclaw");
        git_repo(&root);
        let first = take_notice("ses-notice", &root).expect("first search is told");
        assert!(first
            .starts_with("semantic search is off for openclaw: it is not open in any session."));
        assert!(first.contains("index.roots in ~/.config/cortexkit/aft.jsonc"));
        assert_eq!(take_notice("ses-notice", &root), None);
        assert!(take_notice("ses-other", &root).is_some());
    }
}
