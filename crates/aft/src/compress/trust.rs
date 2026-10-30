//! Trust state for project-supplied TOML compression filters.
//!
//! Project filters at `<project>/.cortexkit/aft/filters/*.toml` are off by default
//! because a malicious repository could ship a filter that lies about output
//! (e.g. strips real test failures and replaces them with `tests: ok`).
//!
//! Trust is keyed by canonicalized project root path. Users opt in via
//! `aft doctor filters trust` (CLI) which calls into [`trust_project`] /
//! [`untrust_project`].
//!
//! The trust file lives at `<storage_dir>/trusted-filter-projects.json` so
//! it survives across bridge restarts and OpenCode restarts.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::fs_lock;

const TRUST_FILE: &str = "trusted-filter-projects.json";
static TRUST_SAVE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustState {
    /// Canonicalized absolute project root paths the user has explicitly
    /// trusted to load `.cortexkit/aft/filters/*.toml`.
    trusted_projects: BTreeSet<String>,
}

/// Returns true when the project root is in the trusted set.
///
/// Returns false (untrusted) on any error — fail-closed by design.
pub fn is_project_trusted(storage_dir: Option<&Path>, project_root: &Path) -> bool {
    let Some(storage_dir) = storage_dir else {
        return false;
    };
    let Ok(canonical) = project_root.canonicalize() else {
        return false;
    };
    let Ok(state) = load(storage_dir) else {
        return false;
    };
    state
        .trusted_projects
        .contains(&canonical.display().to_string())
}

/// Returns true when the project root, or the main checkout of the linked
/// worktree it is, is in the trusted set.
///
/// Bash environment values use this rule: a linked worktree of a trusted
/// checkout inherits its trust, because workers run their builds in exactly
/// those checkouts and the deny list, not trust, is what protects AFT's
/// controls. Compression filters deliberately keep the exact-root rule
/// (`is_project_trusted`) and do not inherit.
pub fn is_project_env_trusted(storage_dir: Option<&Path>, project_root: &Path) -> bool {
    if is_project_trusted(storage_dir, project_root) {
        return true;
    }
    match main_checkout_of(project_root) {
        Some(main) => is_project_trusted(storage_dir, &main),
        None => false,
    }
}

/// The main checkout of a linked worktree, or `None` for a main checkout, a
/// non-repository, or any failure.
///
/// A linked worktree's `.git` is a file holding
/// `gitdir: <main>/.git/worktrees/<name>`, and git pairs it with a back-pointer
/// file `<main>/.git/worktrees/<name>/gitdir` holding the worktree's own `.git`
/// path. This inherits trust only when both halves agree: the entry the first
/// pointer names lives inside the main checkout it names, and the back-pointer
/// names this exact project's `.git`. A folder that merely ships a `.git` file
/// pointing at some trusted checkout has no such back-pointer and is not that
/// checkout's worktree.
///
/// Everything here is bounded to small reads and `canonicalize`; no git process
/// runs, so a bash call costs a few file reads.
fn main_checkout_of(project_root: &Path) -> Option<PathBuf> {
    // A project reached through a symlink is not the checkout it points at:
    // its `.git` belongs to the target, so trusting the link would trust a
    // folder the user never approved.
    refuse_symlinks(project_root)?;
    let marker = project_root.join(".git");
    let pointer = read_bounded(&marker)?;
    let git_dir = resolve_pointer(project_root, pointer.trim().strip_prefix("gitdir:")?)?;
    // `<main>/.git/worktrees/<name>` is the only shape git pairs this way.
    let entry_name = git_dir.file_name()?;
    let worktrees = git_dir.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    let common_dir = worktrees.parent()?.canonicalize().ok()?;
    // The main checkout is the directory that carries `.git`, which is the
    // repository directory's own parent; deriving it from a canonicalized
    // parent-of-parent could step above the checkout when temp roots nest.
    let main = common_dir.parent()?;
    let entry = worktrees.join(entry_name).canonicalize().ok()?;
    // The entry git manages must live inside that main checkout, not wherever
    // a forged pointer or a symlink sends it.
    if !entry.starts_with(&main) {
        return None;
    }
    // git writes the back-pointer into the paired entry; without one naming
    // this project, the worktree does not belong to that checkout. git's own
    // copy ends with a newline, and some tools write the bare path instead.
    let back_pointer = read_bounded(&entry.join("gitdir"))?;
    let claimed = resolve_pointer(
        &entry,
        back_pointer.trim().trim_start_matches("gitdir:").trim(),
    )?;
    // The back-pointer names a checkout's `.git` marker: compare the folder
    // that carries it with this project.
    let claimed_root = claimed.parent()?.canonicalize().ok()?;
    if claimed_root != project_root.canonicalize().ok()? {
        return None;
    }
    // Each side must be this folder's own marker, not a link to another
    // checkout's. The back-pointer can just as easily name a plain folder, so
    // without this a folder whose `.git` links to a real worktree's marker
    // would compare equal and inherit that checkout's trust.
    refuse_symlinks(&marker)?;
    refuse_symlinks(&claimed)?;
    Some(main.to_path_buf())
}

/// Refuse `path` when it is a symlink. Uses `symlink_metadata`, so the check
/// never follows the link it is refusing.
fn refuse_symlinks(path: &Path) -> Option<()> {
    let metadata = fs::symlink_metadata(path).ok()?;
    (!metadata.file_type().is_symlink()).then_some(())
}

/// Resolve a `gitdir:`/`gitdir` pointer target against the directory holding
/// the file it was read from.
fn resolve_pointer(base: &Path, target: &str) -> Option<PathBuf> {
    let target = target.trim();
    if target.is_empty() {
        return None;
    }
    // Git for Windows writes `C:/Users/me/repo/.git` with forward slashes, which
    // the platform path parser does not treat as an absolute prefix.
    #[cfg(windows)]
    {
        let mut chars = target.chars();
        if matches!(chars.next(), Some(c) if c.is_ascii_alphabetic()) && chars.next() == Some(':') {
            return Some(PathBuf::from(target));
        }
    }
    let target = Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        base.join(target)
    })
}

/// Read a small pointer file. The cap keeps a hostile multi-gigabyte file from
/// turning one trust check into a large read.
fn read_bounded(path: &Path) -> Option<String> {
    const MAX_POINTER_BYTES: u64 = 8 * 1024;
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_POINTER_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

/// Add a project to the trusted set. Returns Ok if the file was updated (or
/// already trusted).
pub fn trust_project(storage_dir: &Path, project_root: &Path) -> Result<(), String> {
    let canonical = project_root
        .canonicalize()
        .map_err(|e| format!("canonicalize {}: {e}", project_root.display()))?;
    with_trust_lock(storage_dir, || {
        let mut state = load(storage_dir).unwrap_or_default();
        state
            .trusted_projects
            .insert(canonical.display().to_string());
        save(storage_dir, &state)
    })
}

/// Remove a project from the trusted set. No-op if the project wasn't trusted.
pub fn untrust_project(storage_dir: &Path, project_root: &Path) -> Result<(), String> {
    let canonical = project_root
        .canonicalize()
        .map_err(|e| format!("canonicalize {}: {e}", project_root.display()))?;
    with_trust_lock(storage_dir, || {
        let mut state = load(storage_dir).unwrap_or_default();
        state
            .trusted_projects
            .remove(&canonical.display().to_string());
        save(storage_dir, &state)
    })
}

/// List all trusted project paths (for `aft doctor filters trust --list`).
pub fn list_trusted(storage_dir: &Path) -> Vec<String> {
    load(storage_dir)
        .map(|state| state.trusted_projects.into_iter().collect())
        .unwrap_or_default()
}

fn trust_path(storage_dir: &Path) -> PathBuf {
    storage_dir.join(TRUST_FILE)
}

fn trust_lock_path(storage_dir: &Path) -> PathBuf {
    trust_path(storage_dir).with_extension("json.lock")
}

fn with_trust_lock<T>(
    storage_dir: &Path,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    crate::private_storage::open_root(storage_dir)
        .map_err(|e| format!("create_dir_all {}: {e}", storage_dir.display()))?;
    let lock_path = trust_lock_path(storage_dir);
    let _guard = fs_lock::acquire(&lock_path)
        .map_err(|e| format!("acquire trust lock {}: {e}", lock_path.display()))?;
    action()
}

fn load(storage_dir: &Path) -> Result<TrustState, String> {
    let path = trust_path(storage_dir);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TrustState::default());
        }
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))
}

fn save(storage_dir: &Path, state: &TrustState) -> Result<(), String> {
    crate::private_storage::open_root(storage_dir)
        .map_err(|e| format!("create_dir_all {}: {e}", storage_dir.display()))?;
    let path = trust_path(storage_dir);
    let tmp_path = save_temp_path(&path);
    let bytes =
        serde_json::to_vec_pretty(state).map_err(|e| format!("serialize trust state: {e}"))?;
    crate::private_storage::write(&tmp_path, &bytes)
        .map_err(|e| format!("write {}: {e}", tmp_path.display()))?;
    if let Err(e) = fs::rename(&tmp_path, &path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!(
            "rename {} → {}: {e}",
            tmp_path.display(),
            path.display()
        ));
    }
    Ok(())
}

fn save_temp_path(path: &Path) -> PathBuf {
    let seq = TRUST_SAVE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    path.with_extension(format!("json.tmp.{}.{}.{}", std::process::id(), nanos, seq))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use tempfile::tempdir;

    #[test]
    fn untrusted_by_default() {
        let storage = tempdir().unwrap();
        let project = tempdir().unwrap();
        assert!(!is_project_trusted(Some(storage.path()), project.path()));
    }

    #[test]
    fn trust_then_check() {
        let storage = tempdir().unwrap();
        let project = tempdir().unwrap();
        trust_project(storage.path(), project.path()).unwrap();
        assert!(is_project_trusted(Some(storage.path()), project.path()));
    }

    #[test]
    fn untrust_removes_from_set() {
        let storage = tempdir().unwrap();
        let project = tempdir().unwrap();
        trust_project(storage.path(), project.path()).unwrap();
        untrust_project(storage.path(), project.path()).unwrap();
        assert!(!is_project_trusted(Some(storage.path()), project.path()));
    }

    #[test]
    fn list_returns_trusted_projects() {
        let storage = tempdir().unwrap();
        let p1 = tempdir().unwrap();
        let p2 = tempdir().unwrap();
        trust_project(storage.path(), p1.path()).unwrap();
        trust_project(storage.path(), p2.path()).unwrap();
        let listed = list_trusted(storage.path());
        assert_eq!(listed.len(), 2);
    }

    #[test]
    fn concurrent_trust_project_writes_preserve_both_projects() {
        let storage = tempdir().unwrap();
        let p1 = tempdir().unwrap();
        let p2 = tempdir().unwrap();
        let p1_canonical = p1.path().canonicalize().unwrap().display().to_string();
        let p2_canonical = p2.path().canonicalize().unwrap().display().to_string();

        let barrier = Arc::new(Barrier::new(3));
        let storage_for_t1 = storage.path().to_path_buf();
        let storage_for_t2 = storage.path().to_path_buf();
        let p1_for_thread = p1.path().to_path_buf();
        let p2_for_thread = p2.path().to_path_buf();

        let t1_barrier = Arc::clone(&barrier);
        let t1 = thread::spawn(move || {
            t1_barrier.wait();
            trust_project(&storage_for_t1, &p1_for_thread).unwrap();
        });

        let t2_barrier = Arc::clone(&barrier);
        let t2 = thread::spawn(move || {
            t2_barrier.wait();
            trust_project(&storage_for_t2, &p2_for_thread).unwrap();
        });

        barrier.wait();
        t1.join().unwrap();
        t2.join().unwrap();

        let listed = list_trusted(storage.path());
        assert!(listed.contains(&p1_canonical), "listed: {listed:?}");
        assert!(listed.contains(&p2_canonical), "listed: {listed:?}");
    }

    #[test]
    fn no_storage_dir_means_untrusted() {
        let project = tempdir().unwrap();
        assert!(!is_project_trusted(None, project.path()));
        assert!(!is_project_env_trusted(None, project.path()));
    }

    #[test]
    fn missing_trust_file_is_untrusted_not_error() {
        let storage = tempdir().unwrap();
        let project = tempdir().unwrap();
        // No prior trust calls; file doesn't exist.
        assert!(!is_project_trusted(Some(storage.path()), project.path()));
    }

    #[test]
    fn corrupt_trust_file_is_untrusted() {
        let storage = tempdir().unwrap();
        fs::write(trust_path(storage.path()), b"not valid json").unwrap();
        let project = tempdir().unwrap();
        assert!(!is_project_trusted(Some(storage.path()), project.path()));
    }

    // --- linked worktrees -------------------------------------------------
    //
    // A worktree inherits trust only from the pairing git writes: the
    // worktree's `.git` names `<main>/.git/worktrees/<name>` and that entry's
    // `gitdir` file names the worktree's own `.git` back. Every fixture below
    // is a real repository, so the tests fail loudly if git cannot create one.

    /// A trusted main checkout with an empty `.git` directory and no worktree.
    fn trusted_main(storage: &Path, main: &Path) {
        fs::create_dir_all(main.join(".git")).unwrap();
        trust_project(storage, main).unwrap();
    }

    fn git(args: &[&str], cwd: &Path) -> std::process::Output {
        let mut command = crate::effective_path::new_command("git").build();
        command
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap_or_else(|error| panic!("run git {args:?} in {}: {error}", cwd.display()))
    }

    /// A main checkout with one real linked worktree created by git, returned
    /// beside the directory that holds it (dropping that directory would
    /// delete the worktree).
    fn real_linked_worktree(main: &Path, parent: &Path) -> (tempfile::TempDir, PathBuf) {
        let init = git(&["init", "-q"], main);
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        let commit = git(
            &[
                "-c",
                "user.email=t@example.test",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "base",
            ],
            main,
        );
        assert!(
            commit.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&commit.stderr)
        );
        let holder = tempdir().unwrap();
        let worktree = parent.join("linked");
        let add = git(
            &[
                "worktree",
                "add",
                "-q",
                worktree.to_str().unwrap(),
                "-b",
                "linked",
            ],
            main,
        );
        assert!(
            add.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
        assert!(
            worktree.join(".git").is_file(),
            "a linked worktree carries a `.git` file"
        );
        (holder, worktree)
    }

    /// A trusted main checkout plus a real linked worktree of it.
    fn trusted_checkout_with_worktree() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        tempfile::TempDir,
        PathBuf,
    ) {
        let storage = tempdir().unwrap();
        let main = tempdir().unwrap();
        let parent = tempdir().unwrap();
        let (_holder, worktree) = real_linked_worktree(main.path(), parent.path());
        trust_project(storage.path(), main.path()).unwrap();
        (storage, main, parent, worktree)
    }

    #[test]
    fn a_real_linked_worktree_of_a_trusted_checkout_inherits_trust() {
        let (storage, _main, _parent, worktree) = trusted_checkout_with_worktree();
        // The exact-root filter rule still sees only the worktree itself, so
        // this proves the inheritance decision and not a direct hit.
        assert!(!is_project_trusted(Some(storage.path()), &worktree));
        assert!(is_project_env_trusted(Some(storage.path()), &worktree));
    }

    #[test]
    fn a_forged_git_file_without_a_back_pointer_does_not_inherit_trust() {
        // An unpacked tarball can ship this: a `.git` file aiming at a trusted
        // checkout's worktree entry. git never paired that entry with this
        // folder, so there is no back-pointer naming it.
        let storage = tempdir().unwrap();
        let main = tempdir().unwrap();
        trusted_main(storage.path(), main.path());
        let entry = main.path().join(".git").join("worktrees").join("forged");
        fs::create_dir_all(&entry).unwrap();

        let forged = tempdir().unwrap();
        fs::write(
            forged.path().join(".git"),
            format!("gitdir: {}\n", entry.display()),
        )
        .unwrap();
        assert!(
            !is_project_env_trusted(Some(storage.path()), forged.path()),
            "a `.git` file with no paired back-pointer must not inherit trust"
        );

        // A back-pointer naming a different folder is refused too.
        let elsewhere = tempdir().unwrap();
        fs::write(
            entry.join("gitdir"),
            format!("{}\n", elsewhere.path().join(".git").display()),
        )
        .unwrap();
        assert!(
            !is_project_env_trusted(Some(storage.path()), forged.path()),
            "a back-pointer naming another folder must not inherit trust"
        );

        // Nor is one that names the forged folder itself instead of its `.git`.
        fs::write(
            entry.join("gitdir"),
            format!("{}\n", forged.path().display()),
        )
        .unwrap();
        assert!(
            !is_project_env_trusted(Some(storage.path()), forged.path()),
            "a back-pointer must name this project's `.git`, not the folder"
        );
    }

    #[test]
    fn an_entry_outside_the_named_checkout_does_not_inherit_trust() {
        // A real trusted checkout, so "inside the checkout" means what it means
        // in practice: inside the repository directory of an actual git repo.
        let storage = tempdir().unwrap();
        let main = tempdir().unwrap();
        let parent = tempdir().unwrap();
        let (_holder, worktree) = real_linked_worktree(main.path(), parent.path());
        trust_project(storage.path(), main.path()).unwrap();
        assert!(is_project_env_trusted(Some(storage.path()), &worktree));

        // The worktree's `.git` names an entry that leaves the checkout it
        // claims through `..`, so it does not resolve inside that checkout.
        let outside = main
            .path()
            .join("..")
            .join("aft-test-outside-entry")
            .join("worktrees")
            .join("x");
        fs::create_dir_all(&outside).unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", outside.display()),
        )
        .unwrap();
        assert!(
            !is_project_env_trusted(Some(storage.path()), &worktree),
            "an entry that resolves outside the named checkout must not inherit trust"
        );

        // The same holds when a symlink inside the checkout points at a
        // directory outside it, even with a correct back-pointer in the target.
        #[cfg(unix)]
        {
            let target_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/aft-trust-symlink-entry");
            let _ = fs::remove_dir_all(&target_root);
            fs::create_dir_all(&target_root).unwrap();
            fs::write(
                target_root.join("gitdir"),
                format!("{}\n", worktree.join(".git").display()),
            )
            .unwrap();
            let link = main.path().join(".git").join("worktrees").join("via-link");
            std::os::unix::fs::symlink(&target_root, &link).unwrap();
            fs::write(
                worktree.join(".git"),
                format!("gitdir: {}\n", link.display()),
            )
            .unwrap();
            assert!(
                !is_project_env_trusted(Some(storage.path()), &worktree),
                "a symlinked entry that resolves outside the checkout must not inherit trust"
            );
            let _ = fs::remove_dir_all(&target_root);
        }
    }

    #[test]
    fn a_sibling_folder_reusing_a_worktrees_entry_does_not_inherit_trust() {
        let (storage, _main, _parent, worktree) = trusted_checkout_with_worktree();
        let pointer = fs::read_to_string(worktree.join(".git")).unwrap();
        let entry = pointer
            .trim()
            .trim_start_matches("gitdir:")
            .trim()
            .to_string();

        // A second folder points at the same entry; the entry names `worktree`.
        let sibling = tempdir().unwrap();
        fs::write(sibling.path().join(".git"), format!("gitdir: {entry}\n")).unwrap();
        assert!(!is_project_env_trusted(
            Some(storage.path()),
            sibling.path()
        ));
    }

    #[test]
    fn an_oversized_pointer_file_is_refused() {
        let (storage, _main, _parent, worktree) = trusted_checkout_with_worktree();
        assert!(is_project_env_trusted(Some(storage.path()), &worktree));

        let mut bytes = b"gitdir: ".to_vec();
        bytes.resize(64 * 1024, b' ');
        fs::write(worktree.join(".git"), &bytes).unwrap();
        assert!(
            !is_project_env_trusted(Some(storage.path()), &worktree),
            "a pointer file above the cap must be refused unread"
        );
    }

    #[test]
    fn linked_worktree_of_an_untrusted_checkout_stays_untrusted() {
        let storage = tempdir().unwrap();
        let main = tempdir().unwrap();
        let parent = tempdir().unwrap();
        let (_holder, worktree) = real_linked_worktree(main.path(), parent.path());
        let other = tempdir().unwrap();
        trust_project(storage.path(), other.path()).unwrap();
        assert!(!is_project_env_trusted(Some(storage.path()), &worktree));
    }

    // Creating symlinks needs elevated rights on Windows, so these two run on
    // Unix only; the guards they exercise are platform-independent.
    #[cfg(unix)]
    #[test]
    fn a_marker_reached_through_a_symlink_is_refused_outright() {
        // The attack: a folder whose `.git` is a symlink to a real linked
        // worktree's `.git`. Reading through the link yields that worktree's
        // pairing, so the check must refuse the linked marker itself rather
        // than compare paths it derived through the link.
        let (storage, _main, _parent, worktree) = trusted_checkout_with_worktree();
        assert!(is_project_env_trusted(Some(storage.path()), &worktree));

        let attacker = tempdir().unwrap();
        let marker = attacker.path().join(".git");
        std::os::unix::fs::symlink(worktree.join(".git"), &marker).unwrap();
        assert!(
            refuse_symlinks(&marker).is_none(),
            "a symlinked marker must be refused by the marker check"
        );
        assert!(
            main_checkout_of(attacker.path()).is_none(),
            "a folder whose marker is a link has no checkout of its own"
        );
        assert!(
            !is_project_env_trusted(Some(storage.path()), attacker.path()),
            "a project whose `.git` is a symlink must not inherit trust"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_worktree_root_reached_through_a_symlink_is_refused() {
        // The same trick one level up: the whole project path is a link to the
        // real worktree, so its `.git` is reached through that link.
        let (storage, _main, _parent, worktree) = trusted_checkout_with_worktree();
        let holder = tempdir().unwrap();
        let linked_root = holder.path().join("linked-root");
        std::os::unix::fs::symlink(&worktree, &linked_root).unwrap();
        assert!(
            !is_project_env_trusted(Some(storage.path()), &linked_root),
            "a worktree reached through a symlinked root must not inherit trust"
        );
    }

    #[test]
    fn main_checkout_has_no_parent_checkout_to_inherit_from() {
        let main = tempdir().unwrap();
        fs::create_dir_all(main.path().join(".git")).unwrap();
        assert!(main_checkout_of(main.path()).is_none());
    }
}
