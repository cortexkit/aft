//! Disclosure for callgraph answers served from another checkout's graph.
//!
//! A linked worktree, or a second clone of the same repository, does not build
//! its own callgraph store: it reads the one the owning checkout publishes
//! (borrow-only mode, see `artifact_owner`). That graph describes the owner's
//! files. When this checkout sits on another commit or has edits of its own,
//! callers, line numbers and even whether a symbol exists can differ from the
//! files here, and an unmarked answer would look complete and current.
//!
//! Until each checkout gets its own graph view, every callgraph query answered
//! from a borrowed store checks whether the borrowed graph can differ from this
//! checkout and, when it can, marks the answer `complete: false` with a
//! one-line explanation. The check uses only cheap signals: the two checkouts'
//! HEAD commits, read straight from the git metadata files, and the number of
//! files the search index's RAM overlay already knows differ from the owner's
//! snapshot. It never walks the working tree.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::artifact_owner::ArtifactOwnerMode;
use crate::context::AppContext;
use crate::protocol::Response;

/// Where the queried symbol of an operation is looked up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolLookup {
    /// The symbol is resolved in the callgraph store, so a not-found answer
    /// can mean only that the borrowed graph lacks it.
    Graph,
    /// The symbol is resolved by parsing this checkout's own file, so a
    /// not-found answer is already about this checkout.
    Checkout,
}

/// Error codes that report a symbol missing from the graph.
const GRAPH_NOT_FOUND_CODES: [&str; 2] = ["symbol_not_found", "target_symbol_not_found"];

/// How a borrowed callgraph relates to this checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BorrowedCallgraph {
    /// The checkout whose graph is being read; `None` when it is unknown.
    pub owner_checkout: Option<PathBuf>,
    /// The owner checkout's current HEAD commit.
    pub owner_head: Option<String>,
    /// This checkout's HEAD commit.
    pub checkout_head: Option<String>,
    /// Files here whose content differs from the owner's indexed snapshot;
    /// `None` when that is not known.
    pub changed_files: Option<usize>,
}

impl BorrowedCallgraph {
    /// True unless the borrowed graph provably describes this checkout: both
    /// checkouts are on the same known commit and no file here is known to
    /// differ from what the owner indexed. Anything unknown counts as a
    /// possible difference.
    pub fn can_differ(&self) -> bool {
        let same_head = matches!(
            (&self.owner_head, &self.checkout_head),
            (Some(owner), Some(checkout)) if owner == checkout
        );
        !(same_head && self.changed_files == Some(0))
    }

    fn provenance(&self) -> String {
        let owner = self
            .owner_checkout
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "another checkout".to_string());
        let changed = match self.changed_files {
            Some(1) => "1 changed file".to_string(),
            Some(count) => format!("{count} changed files"),
            None => "an unknown number of changed files".to_string(),
        };
        format!(
            "borrowed from {owner} at {}; this checkout is at {} with {changed}",
            short_commit(self.owner_head.as_deref()),
            short_commit(self.checkout_head.as_deref()),
        )
    }

    /// The single line added to a successful answer.
    pub fn summary_line(&self) -> String {
        format!(
            "callgraph: {}, so callers and line numbers may not match this tree",
            self.provenance()
        )
    }

    /// The clause appended to a not-found error from the borrowed graph.
    pub fn not_found_hint(&self) -> String {
        format!(
            "the callgraph is {}, so the symbol may exist in this checkout but not in the borrowed graph",
            self.provenance()
        )
    }

    fn to_json(&self) -> Value {
        json!({
            "owner_checkout": self.owner_checkout.as_ref().map(|path| path.display().to_string()),
            "owner_head": self.owner_head,
            "checkout_head": self.checkout_head,
            "changed_files": self.changed_files,
            "message": self.summary_line(),
        })
    }
}

fn short_commit(commit: Option<&str>) -> String {
    match commit {
        // Object ids are ASCII hex, so the first seven bytes are the usual
        // abbreviated commit.
        Some(commit) => commit.get(..7).unwrap_or(commit).to_string(),
        None => "an unknown commit".to_string(),
    }
}

/// Annotate a callgraph response that was answered from a borrowed store
/// whose graph can differ from this checkout. Successful answers get
/// `complete: false` and a `borrowed_callgraph` object; not-found errors for a
/// symbol looked up in the graph get a hint appended to their message. Every
/// other response, and every response on the owner checkout or on a
/// per-checkout view, is left exactly as it was.
pub fn disclose_borrowed_answer(ctx: &AppContext, response: &mut Response, lookup: SymbolLookup) {
    let graph_not_found = lookup == SymbolLookup::Graph
        && !response.success
        && response
            .data
            .get("code")
            .and_then(Value::as_str)
            .is_some_and(|code| GRAPH_NOT_FOUND_CODES.contains(&code));
    if !response.success && !graph_not_found {
        return;
    }
    let Some(borrowed) = borrowed_callgraph(ctx).filter(BorrowedCallgraph::can_differ) else {
        return;
    };
    let Some(data) = response.data.as_object_mut() else {
        return;
    };
    if response.success {
        data.insert("complete".to_string(), Value::Bool(false));
        data.insert("borrowed_callgraph".to_string(), borrowed.to_json());
        return;
    }
    let message = data
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_string();
    data.insert(
        "message".to_string(),
        Value::String(format!("{message}; {}", borrowed.not_found_hint())),
    );
}

/// Describe the borrowed callgraph this context answers from, or `None` when
/// callgraph answers come from this checkout's own data: the owner checkout,
/// a per-checkout query runtime, or a pinned per-HEAD view.
pub fn borrowed_callgraph(ctx: &AppContext) -> Option<BorrowedCallgraph> {
    if !ctx.shared_artifacts_read_only() || ctx.checkout_query_runtime_active() {
        return None;
    }
    if ctx.config().views.enabled
        && ctx
            .pinned_view_runtime()
            .is_some_and(|view| view.manifest.is_some())
    {
        return None;
    }
    let checkout_root = ctx.config().project_root.clone()?;
    let checkout_root = canonical(&checkout_root);
    let owner_checkout = match owner_checkout(ctx, &checkout_root) {
        Owner::Other(path) => Some(path),
        Owner::Unknown => None,
        Owner::ThisCheckout => return None,
    };
    Some(BorrowedCallgraph {
        owner_head: owner_checkout.as_deref().and_then(checkout_head),
        owner_checkout,
        checkout_head: checkout_head(&checkout_root),
        changed_files: overlay_changed_files(ctx),
    })
}

enum Owner {
    Other(PathBuf),
    Unknown,
    ThisCheckout,
}

fn owner_checkout(ctx: &AppContext, checkout_root: &Path) -> Owner {
    let recorded = ctx
        .artifact_owner_status()
        .filter(|status| status.mode == ArtifactOwnerMode::ReadOnly)
        .map(|status| canonical(Path::new(&status.owner_checkout_path)));
    if let Some(owner) = recorded.filter(|owner| owner != checkout_root) {
        return Owner::Other(owner);
    }
    // A read-only checkout that is not a linked worktree and names itself as
    // the owner (for example, artifacts written by a newer build) reads its
    // own data, so there is nothing to disclose.
    if !ctx.is_worktree_bridge() {
        return Owner::ThisCheckout;
    }
    // A linked worktree whose owner manifest was unreadable falls back to its
    // own path. The graph still comes from elsewhere; the main worktree, the
    // parent of the shared `.git` directory, is the usual owner.
    ctx.git_common_dir()
        .filter(|dir| dir.file_name().is_some_and(|name| name == ".git"))
        .and_then(|dir| dir.parent().map(canonical))
        .filter(|main| main != checkout_root)
        .map_or(Owner::Unknown, Owner::Other)
}

/// Files the search index's RAM overlay has found to differ from the owner's
/// snapshot. Known only while the overlay is on and the reconciled borrowed
/// index is installed; without the overlay the borrowed index is never
/// compared with this checkout, so an empty delta would prove nothing.
fn overlay_changed_files(ctx: &AppContext) -> Option<usize> {
    if !ctx.ram_overlay_active() {
        return None;
    }
    let guard = ctx
        .search_index()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let index = guard.as_ref()?;
    (index.ready && !index.build_denied).then(|| index.overlay_changed_file_count())
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Resolve a checkout's HEAD commit from its git metadata files without
/// running git: `.git` (a directory, or a `gitdir:` file in a linked
/// worktree), `HEAD`, loose refs, and `packed-refs`. Returns `None` for
/// anything it cannot resolve, such as a reftable repository.
pub fn checkout_head(checkout: &Path) -> Option<String> {
    let dot_git = checkout.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let marker = fs::read_to_string(&dot_git).ok()?;
        let target = PathBuf::from(marker.trim().strip_prefix("gitdir:")?.trim());
        if target.is_absolute() {
            target
        } else {
            checkout.join(target)
        }
    };
    // A linked worktree keeps HEAD in its own git dir and shares branch refs
    // through the directory named by `commondir`.
    let common_dir = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) => {
            let path = PathBuf::from(text.trim());
            if path.is_absolute() {
                path
            } else {
                git_dir.join(path)
            }
        }
        Err(_) => git_dir.clone(),
    };
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    resolve_ref_value(&git_dir, &common_dir, head.trim(), 0)
}

/// Resolve the text of a ref file: an object id, or `ref: <name>` pointing at
/// another ref. Symbolic chains are followed a few levels at most.
fn resolve_ref_value(git_dir: &Path, common_dir: &Path, value: &str, depth: u8) -> Option<String> {
    let Some(reference) = value.strip_prefix("ref:").map(str::trim) else {
        return is_object_id(value).then(|| value.to_string());
    };
    if depth >= 5 || reference.is_empty() {
        return None;
    }
    for dir in [git_dir, common_dir] {
        if let Ok(text) = fs::read_to_string(dir.join(reference)) {
            return resolve_ref_value(git_dir, common_dir, text.trim(), depth + 1);
        }
    }
    let packed = fs::read_to_string(common_dir.join("packed-refs")).ok()?;
    packed
        .lines()
        .filter(|line| !line.starts_with('#') && !line.starts_with('^'))
        .filter_map(|line| line.split_once(' '))
        .find(|(_, name)| name.trim() == reference)
        .map(|(id, _)| id.trim())
        .filter(|id| is_object_id(id))
        .map(str::to_string)
}

fn is_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn borrowed(
        owner_head: Option<&str>,
        checkout_head: Option<&str>,
        changed_files: Option<usize>,
    ) -> BorrowedCallgraph {
        BorrowedCallgraph {
            owner_checkout: Some(PathBuf::from("/work/owner")),
            owner_head: owner_head.map(str::to_string),
            checkout_head: checkout_head.map(str::to_string),
            changed_files,
        }
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn only_a_known_same_head_with_no_changed_files_proves_the_graph_fits() {
        assert!(!borrowed(Some(A), Some(A), Some(0)).can_differ());
        assert!(borrowed(Some(A), Some(B), Some(0)).can_differ());
        assert!(borrowed(Some(A), Some(A), Some(2)).can_differ());
        assert!(borrowed(Some(A), Some(A), None).can_differ());
        assert!(borrowed(None, Some(A), Some(0)).can_differ());
        assert!(borrowed(None, None, Some(0)).can_differ());
    }

    #[test]
    fn disclosure_lines_are_single_lines_naming_both_commits_and_the_change_count() {
        let line = borrowed(Some(A), Some(B), Some(3)).summary_line();
        assert_eq!(
            line,
            "callgraph: borrowed from /work/owner at aaaaaaa; this checkout is at bbbbbbb with 3 changed files, so callers and line numbers may not match this tree"
        );
        let unknown = BorrowedCallgraph {
            owner_checkout: None,
            owner_head: None,
            checkout_head: Some(B.to_string()),
            changed_files: None,
        }
        .not_found_hint();
        assert_eq!(
            unknown,
            "the callgraph is borrowed from another checkout at an unknown commit; this checkout is at bbbbbbb with an unknown number of changed files, so the symbol may exist in this checkout but not in the borrowed graph"
        );
        assert!(!line.contains('\n') && !unknown.contains('\n'));
        assert!(borrowed(Some(A), Some(B), Some(1))
            .summary_line()
            .contains("with 1 changed file,"));
    }

    #[test]
    fn head_resolves_detached_loose_packed_and_linked_worktree_refs() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        let git = main.join(".git");
        fs::create_dir_all(git.join("refs/heads")).unwrap();

        fs::write(git.join("HEAD"), format!("{A}\n")).unwrap();
        assert_eq!(checkout_head(&main).as_deref(), Some(A));

        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(git.join("refs/heads/main"), format!("{B}\n")).unwrap();
        assert_eq!(checkout_head(&main).as_deref(), Some(B));

        fs::remove_file(git.join("refs/heads/main")).unwrap();
        fs::write(
            git.join("packed-refs"),
            format!("# pack-refs with: peeled fully-peeled sorted\n{A} refs/heads/main\n^{B}\n"),
        )
        .unwrap();
        assert_eq!(checkout_head(&main).as_deref(), Some(A));

        let linked_git = git.join("worktrees/linked");
        fs::create_dir_all(&linked_git).unwrap();
        fs::write(linked_git.join("commondir"), "../..\n").unwrap();
        fs::write(linked_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let linked = temp.path().join("linked");
        fs::create_dir_all(&linked).unwrap();
        fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", linked_git.display()),
        )
        .unwrap();
        assert_eq!(checkout_head(&linked).as_deref(), Some(A));

        fs::write(linked_git.join("HEAD"), "ref: refs/heads/missing\n").unwrap();
        assert_eq!(checkout_head(&linked), None);
        assert_eq!(checkout_head(&temp.path().join("absent")), None);
    }
}
