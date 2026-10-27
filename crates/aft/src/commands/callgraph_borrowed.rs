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
//! one-line explanation. The check uses only signals already at hand: the two
//! checkouts' HEAD commits, read straight from the git metadata files, and
//! the per-file content hashes the callgraph store recorded, compared with
//! the hashes the search index's RAM overlay holds for this checkout's files.
//! It never walks the working tree; when the two checkouts share a commit and
//! some files differ, git is asked which of them this checkout has edited.
//!
//! The graph cannot see what those differing files hold here, so an answer
//! that depends on them is extended or qualified: `callers` and `impact`
//! search the differing files for the target's name (a bounded read of only
//! those files) and list what they find as name-matched, and an answer that is
//! still empty names the files the graph does not reflect and suggests grep
//! rather than reading as "nothing calls this".

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::artifact_owner::ArtifactOwnerMode;
use crate::cache_freshness;
use crate::context::AppContext;
use crate::inspect::job::is_test_file;
use crate::language::LanguageProvider;
use crate::protocol::{RawRequest, Response};
use crate::symbols::{Symbol, SymbolKind};

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
    /// Files whose content differs between the source snapshot the borrowed
    /// graph describes and this checkout's disk; `None` when that is not
    /// known.
    pub changed_files: Option<usize>,
    /// Which checkout holds the edits behind those differences, when it is
    /// known.
    pub edits_in: Option<EditsIn>,
    /// The differing files themselves; `None` when nothing is known about
    /// them. The list can be partial, see [`DifferingFiles::complete`].
    pub differing_files: Option<DifferingFiles>,
}

/// Root-relative paths, with `/` separators and sorted, of files whose content
/// differs between the source snapshot a borrowed graph describes and this
/// checkout's disk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DifferingFiles {
    pub files: Vec<String>,
    /// False when the comparison ran out of its disk-read budget before it
    /// had checked every file, so more files may differ than are listed.
    pub complete: bool,
}

/// The navigation operation whose answer is being disclosed. Only `callers`
/// and `impact` answers can be extended by searching differing files for the
/// target's name; the other operations report paths or trees that text cannot
/// rebuild, so their answers only say what the borrowed graph cannot see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BorrowedOp {
    Callers,
    Impact,
    CallTree,
    TraceTo,
    TraceToSymbol,
    TraceData,
}

/// The checkout whose uncommitted edits make the borrowed graph differ from
/// this checkout. Known only when both checkouts are on the same commit, so
/// that every difference is an edit on one side or the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditsIn {
    /// The checkout that owns the graph: this checkout's differing files all
    /// match its commit.
    BorrowedCheckout,
    /// This checkout: every differing file has edits here.
    ThisCheckout,
    /// Some differing files have edits here and some do not.
    BothCheckouts,
}

impl EditsIn {
    fn label(self) -> &'static str {
        match self {
            EditsIn::BorrowedCheckout => "borrowed_checkout",
            EditsIn::ThisCheckout => "this_checkout",
            EditsIn::BothCheckouts => "both_checkouts",
        }
    }

    fn clause(self) -> &'static str {
        match self {
            EditsIn::BorrowedCheckout => " (edits in the borrowed checkout)",
            EditsIn::ThisCheckout => " (edits in this checkout)",
            EditsIn::BothCheckouts => " (edits in both checkouts)",
        }
    }
}

impl BorrowedCallgraph {
    /// True unless the borrowed graph provably describes this checkout: both
    /// checkouts are on the same known commit and no file here is known to
    /// differ from the files the graph was built from. Anything unknown
    /// counts as a possible difference.
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
            // The count is measured against the files the borrowed graph was
            // built from, not against this checkout's own HEAD, so a clean
            // checkout on another commit still has files that differ; the
            // phrase names the graph to make that clear.
            Some(1) => "1 file that differs from the borrowed graph".to_string(),
            Some(count) => format!("{count} files that differ from the borrowed graph"),
            None => "an unknown number of files that differ from the borrowed graph".to_string(),
        };
        let edits_in = match (self.changed_files, self.edits_in) {
            (Some(count), Some(edits_in)) if count > 0 => edits_in.clause(),
            _ => "",
        };
        format!(
            "borrowed from {owner} at {}; this checkout is at {} with {changed}{edits_in}",
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
            "the callgraph is {}, so the symbol may exist in this checkout but not in the borrowed graph; use grep or aft_zoom on this checkout to confirm",
            self.provenance()
        )
    }

    fn to_json(&self) -> Value {
        json!({
            "owner_checkout": self.owner_checkout.as_ref().map(|path| path.display().to_string()),
            "owner_head": self.owner_head,
            "checkout_head": self.checkout_head,
            "changed_files": self.changed_files,
            "edits_in": self.edits_in.map(EditsIn::label),
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
///
/// A successful answer is also checked against the files that differ from
/// the borrowed graph (see [`cover_differing_files`]): `callers` and `impact`
/// gain name-matched entries found by searching those files, and an answer
/// that is still empty says which files the graph could not see instead of
/// reading as a definitive zero.
pub fn disclose_borrowed_answer(
    ctx: &AppContext,
    req: &RawRequest,
    response: &mut Response,
    lookup: SymbolLookup,
    op: BorrowedOp,
) {
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
        let query = CoverageQuery::from_request(req, data);
        let root = ctx
            .config()
            .project_root
            .clone()
            .map(|root| canonical(&root));
        let search = |files: &[String], name: &str| match &root {
            Some(root) => {
                search_differing_files(ctx.provider(), root, files, name, &SearchBounds::DEFAULT)
            }
            None => NameSearch::unsearched(files),
        };
        cover_differing_files(data, &borrowed, op, &query, search);
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
/// a per-checkout query runtime, a checkout's v3 view or a pinned per-HEAD view.
pub fn borrowed_callgraph(ctx: &AppContext) -> Option<BorrowedCallgraph> {
    if !ctx.shared_artifacts_read_only()
        || ctx.checkout_query_runtime_active()
        || ctx.callgraph_views_enabled()
    {
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
    let owner_head = owner_checkout.as_deref().and_then(checkout_head);
    let checkout_head = checkout_head(&checkout_root);
    let differing =
        files_differing_from_borrowed_graph(ctx, &checkout_root, owner_checkout.as_deref());
    // Only a complete list gives a count and lets the edits be located; a
    // partial one is kept for naming and searching the files it does list.
    let complete_list = differing
        .as_ref()
        .filter(|differing| differing.complete)
        .map(|differing| differing.files.as_slice());
    let edits_in = complete_list.and_then(|differing| {
        let same_head = matches!(
            (&owner_head, &checkout_head),
            (Some(owner), Some(checkout)) if owner == checkout
        );
        (same_head && !differing.is_empty())
            .then(|| locate_edits(&checkout_root, differing))
            .flatten()
    });
    Some(BorrowedCallgraph {
        owner_head,
        owner_checkout,
        checkout_head,
        changed_files: complete_list.map(<[String]>::len),
        edits_in,
        differing_files: differing,
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

/// The most files read from disk to decide one disclosure. Nearly every file
/// is compared from hashes already in memory; this bounds the rest (files the
/// search index never hashed, or holds no record of) so a pathological tree
/// costs a bounded amount per answer. When it runs out, the files found so far
/// are kept but the list is marked incomplete and the count reported unknown.
const MAX_DISK_HASHES: usize = 256;

/// The files whose content differs between the source snapshot the borrowed
/// graph describes and this checkout's disk.
///
/// The graph's side is the per-file size and content hash the callgraph store
/// recorded when it parsed each file. This checkout's side comes from the
/// search index's RAM overlay, which has already compared the borrowed search
/// snapshot with this checkout's files and keeps itself current from watcher
/// events, so it holds this checkout's hashes without walking the tree. The
/// overlay's own delta is not the answer: it is measured against the shared
/// search snapshot, which the owner rewrites only on shutdown or a rebuild,
/// so it also counts every file the owner changed since then, and files it
/// never hashed (binary or oversized ones), even when this checkout matches
/// the graph exactly.
///
/// `None` when nothing is known: without the overlay, or before the store or
/// the index is ready. A comparison that would read too many files returns
/// the files it found with `complete: false`.
fn files_differing_from_borrowed_graph(
    ctx: &AppContext,
    checkout_root: &Path,
    owner_root: Option<&Path>,
) -> Option<DifferingFiles> {
    if !ctx.ram_overlay_active() {
        return None;
    }
    let checkout = {
        let guard = ctx
            .search_index()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = guard.as_ref()?;
        if !index.ready || index.build_denied {
            return None;
        }
        index.live_file_identities()
    };
    let store = ctx
        .callgraph_store()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()?;
    let graph = store.file_identities().ok()?;
    Some(compare_with_graph(
        &graph,
        checkout,
        checkout_root,
        owner_root,
        MAX_DISK_HASHES,
    ))
}

/// Size and content hash of one file; a zero hash means it was never hashed.
type FileIdentity = (u64, blake3::Hash);

/// The disk-read budget of one comparison ran out.
struct BudgetSpent;

struct DiskHasher {
    remaining: usize,
}

impl DiskHasher {
    /// Hash the file at `path`; `Ok(None)` when it cannot be read.
    fn hash(&mut self, path: &Path) -> Result<Option<blake3::Hash>, BudgetSpent> {
        self.remaining = self.remaining.checked_sub(1).ok_or(BudgetSpent)?;
        Ok(fs::read(path)
            .ok()
            .map(|bytes| cache_freshness::hash_bytes(&bytes)))
    }

    /// Whether the file at `path`, whose size is `size` and whose hash is
    /// `hash` when it was hashed, holds the content `expected` describes.
    /// When `expected` has no hash (the graph does not hash very large
    /// files) the sizes are all there is to compare.
    fn same_content(
        &mut self,
        expected: FileIdentity,
        size: u64,
        hash: blake3::Hash,
        path: &Path,
    ) -> Result<bool, BudgetSpent> {
        let zero = cache_freshness::zero_hash();
        let (expected_size, expected_hash) = expected;
        if size != expected_size {
            return Ok(false);
        }
        if expected_hash == zero {
            return Ok(true);
        }
        if hash != zero {
            return Ok(hash == expected_hash);
        }
        Ok(self.hash(path)? == Some(expected_hash))
    }
}

/// Compare the graph's recorded files with this checkout's files, both keyed
/// by root-relative path with `/` separators, and return the differing paths
/// sorted.
///
/// A graph file is compared with this checkout's record of the same path or,
/// when this checkout's index has none (its walk can skip what the graph's
/// walk keeps), with the file on disk; a graph file missing here differs. A
/// file here that the graph lacks differs only when the graph's walk would
/// include it and the borrowed checkout lacks it or holds other content: a
/// file both checkouts hold identically is one the graph equally leaves out
/// here, for example a source file its parser rejected.
///
/// When the disk-read budget runs out the comparison stops and returns the
/// files found so far, marked incomplete.
fn compare_with_graph(
    graph: &HashMap<String, FileIdentity>,
    checkout: HashMap<String, FileIdentity>,
    checkout_root: &Path,
    owner_root: Option<&Path>,
    max_disk_hashes: usize,
) -> DifferingFiles {
    let mut hasher = DiskHasher {
        remaining: max_disk_hashes,
    };
    let mut differing = Vec::new();
    let complete = collect_differing(
        graph,
        checkout,
        checkout_root,
        owner_root,
        &mut hasher,
        &mut differing,
    )
    .is_ok();
    differing.sort();
    DifferingFiles {
        files: differing,
        complete,
    }
}

fn collect_differing(
    graph: &HashMap<String, FileIdentity>,
    mut checkout: HashMap<String, FileIdentity>,
    checkout_root: &Path,
    owner_root: Option<&Path>,
    hasher: &mut DiskHasher,
    differing: &mut Vec<String>,
) -> Result<(), BudgetSpent> {
    let zero = cache_freshness::zero_hash();
    // Paths are visited in order so that a comparison cut short by the budget
    // reports the same partial list each time.
    let mut graph_paths: Vec<_> = graph.iter().collect();
    graph_paths.sort_unstable_by(|left, right| left.0.cmp(right.0));
    for (relative, &expected) in graph_paths {
        let path = checkout_root.join(relative);
        let same = match checkout.remove(relative) {
            Some((size, hash)) => hasher.same_content(expected, size, hash, &path)?,
            None => match fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => {
                    hasher.same_content(expected, metadata.len(), zero, &path)?
                }
                _ => false,
            },
        };
        if !same {
            differing.push(relative.clone());
        }
    }
    let mut checkout_paths: Vec<_> = checkout.into_iter().collect();
    checkout_paths.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    for (relative, (size, hash)) in checkout_paths {
        if !crate::callgraph::walk_could_include(&relative) {
            continue;
        }
        let same_as_owner = match owner_root {
            Some(owner_root) => {
                let owner_path = owner_root.join(&relative);
                match fs::metadata(&owner_path) {
                    Ok(metadata) if metadata.is_file() && metadata.len() == size => {
                        let owner_hash = hasher.hash(&owner_path)?;
                        let here = if hash == zero {
                            hasher.hash(&checkout_root.join(&relative))?
                        } else {
                            Some(hash)
                        };
                        owner_hash.is_some() && owner_hash == here
                    }
                    _ => false,
                }
            }
            None => false,
        };
        if !same_as_owner {
            differing.push(relative);
        }
    }
    Ok(())
}

/// Which checkout holds the edits behind `differing`, for two checkouts on
/// the same commit: a differing file this checkout has not changed from that
/// commit must have been changed in the borrowed checkout. Asks git for this
/// checkout's changed and untracked files; `None` when git cannot say.
fn locate_edits(checkout_root: &Path, differing: &[String]) -> Option<EditsIn> {
    let mut edited_here = git_paths(
        checkout_root,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "--relative",
            "-z",
            "HEAD",
        ],
    )?;
    edited_here.extend(git_paths(
        checkout_root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?);
    let here = differing
        .iter()
        .filter(|path| edited_here.contains(*path))
        .count();
    Some(match here {
        0 => EditsIn::BorrowedCheckout,
        here if here == differing.len() => EditsIn::ThisCheckout,
        _ => EditsIn::BothCheckouts,
    })
}

/// Run a git command in `root` that prints NUL-separated paths relative to
/// it, with a short deadline so a slow repository cannot hold up an answer.
fn git_paths(root: &Path, args: &[&str]) -> Option<HashSet<String>> {
    const GIT_DEADLINE: Duration = Duration::from_secs(2);

    if !crate::developer_tools::git_usable() {
        return None;
    }
    let mut child = crate::effective_path::new_command("git")
        .build()
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Drain stdout while waiting: a long path list would otherwise fill the
    // pipe and stall git until the deadline.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + GIT_DEADLINE;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let bytes = reader.join().ok()?.ok()?;
    if !status?.success() {
        return None;
    }
    Some(
        bytes
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).replace('\\', "/"))
            .collect(),
    )
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// What the coverage check needs from the request and the answer.
struct CoverageQuery {
    /// The target's bare name: searched for in differing files and named in
    /// the grep suggestion. `None` when the request carries no symbol.
    name: Option<String>,
    include_tests: bool,
}

impl CoverageQuery {
    fn from_request(req: &RawRequest, data: &Map<String, Value>) -> Self {
        // The answer names the symbol it resolved; the request's spelling is
        // the fallback for answers that do not.
        let name = data
            .get("symbol")
            .and_then(Value::as_str)
            .or_else(|| req.params.get("symbol").and_then(Value::as_str))
            .map(bare_name)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        let include_tests = req
            .params
            .get("includeTests")
            .or_else(|| req.params.get("include_tests"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Self {
            name,
            include_tests,
        }
    }
}

/// The last segment of a qualified symbol (`Type::method`, `Class.method`),
/// which is what a call site spells.
fn bare_name(symbol: &str) -> &str {
    symbol
        .rsplit(['.', ':', '#'])
        .next()
        .unwrap_or(symbol)
        .trim()
}

/// Bounds on searching differing files for a name, so one answer costs a
/// bounded amount however many files differ. The file count matches the
/// disk-read budget of the comparison that lists those files; the deadline
/// keeps the search well under the time an agent waits for a callgraph
/// answer; the match limit matches the cap on name-matched macro callers.
#[derive(Clone, Copy, Debug)]
struct SearchBounds {
    max_files: usize,
    max_file_bytes: u64,
    deadline: Duration,
    max_matches: usize,
}

impl SearchBounds {
    const DEFAULT: Self = Self {
        max_files: 256,
        max_file_bytes: 1024 * 1024,
        deadline: Duration::from_millis(250),
        max_matches: 50,
    };
}

/// The bound that stopped a search before it had read every file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchStop {
    Files,
    Deadline,
    Matches,
}

impl SearchStop {
    fn label(self) -> &'static str {
        match self {
            SearchStop::Files => "files",
            SearchStop::Deadline => "deadline",
            SearchStop::Matches => "matches",
        }
    }
}

/// One line of a differing file that mentions the target's name.
#[derive(Clone, Debug, PartialEq, Eq)]
struct NameMatch {
    file: String,
    /// 1-based line number.
    line: u32,
    /// The innermost symbol enclosing the line, or `<top-level>`.
    symbol: String,
    /// The line's text, trimmed.
    text: String,
    in_test: bool,
}

/// The outcome of searching differing files for the target's name.
#[derive(Debug)]
struct NameSearch {
    matches: Vec<NameMatch>,
    searched: usize,
    /// Files that exist here but were not read: past a bound, too large, or
    /// unreadable.
    not_searched: Vec<String>,
    stopped_by: Option<SearchStop>,
    bounds: SearchBounds,
}

impl NameSearch {
    fn unsearched(files: &[String]) -> Self {
        Self {
            matches: Vec::new(),
            searched: 0,
            not_searched: files.to_vec(),
            stopped_by: None,
            bounds: SearchBounds::DEFAULT,
        }
    }

    /// True when some differing file may hold matches this search missed.
    fn partial(&self) -> bool {
        self.stopped_by.is_some() || !self.not_searched.is_empty()
    }
}

/// Search `files` (root-relative) for lines that use `name` as a whole word,
/// the way the graph's own name-matched callers are found: no resolution, so
/// a namesake matches too. Lines that only import the name, comment lines,
/// and the name's own definitions are skipped. Test files are searched too;
/// whether their matches are listed or counted as hidden is decided when the
/// matches are merged into the answer.
fn search_differing_files(
    provider: &dyn LanguageProvider,
    root: &Path,
    files: &[String],
    name: &str,
    bounds: &SearchBounds,
) -> NameSearch {
    let started = Instant::now();
    let mut search = NameSearch {
        matches: Vec::new(),
        searched: 0,
        not_searched: Vec::new(),
        stopped_by: None,
        bounds: *bounds,
    };
    for (index, relative) in files.iter().enumerate() {
        let stop = if search.searched >= bounds.max_files {
            Some(SearchStop::Files)
        } else if search.matches.len() >= bounds.max_matches {
            Some(SearchStop::Matches)
        } else if started.elapsed() >= bounds.deadline {
            Some(SearchStop::Deadline)
        } else {
            None
        };
        if let Some(stop) = stop {
            search.stopped_by = Some(stop);
            search.not_searched.extend(files[index..].iter().cloned());
            break;
        }
        let path = root.join(relative);
        // A file the graph has but this checkout deleted holds no callers.
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        if metadata.len() > bounds.max_file_bytes {
            search.not_searched.push(relative.clone());
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            search.not_searched.push(relative.clone());
            continue;
        };
        search.searched += 1;
        let text = String::from_utf8_lossy(&bytes);
        let lines = lines_using_name(&text, name);
        if lines.is_empty() {
            continue;
        }
        let symbols = provider.list_symbols(&path).ok();
        let in_test = is_test_file(relative);
        for (line, text) in lines {
            if defines_name_at(symbols.as_deref(), name, line) {
                continue;
            }
            if search.matches.len() >= bounds.max_matches {
                search.stopped_by = Some(SearchStop::Matches);
                search
                    .not_searched
                    .extend(files[index + 1..].iter().cloned());
                return search;
            }
            search.matches.push(NameMatch {
                file: relative.clone(),
                line,
                symbol: enclosing_symbol(symbols.as_deref(), line),
                text: text.trim().to_string(),
                in_test,
            });
        }
    }
    search
}

/// Lines (1-based) of `text` where `name` occurs as a whole word outside a
/// definition keyword, skipping comment and import lines.
fn lines_using_name<'a>(text: &'a str, name: &str) -> Vec<(u32, &'a str)> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| !is_comment_or_import(line.trim_start()))
        .filter(|(_, line)| uses_name(line, name))
        .map(|(index, line)| (u32::try_from(index + 1).unwrap_or(u32::MAX), line))
        .collect()
}

fn is_comment_or_import(trimmed: &str) -> bool {
    const PREFIXES: [&str; 13] = [
        "//",
        "/*",
        "* ",
        "#",
        "--",
        "import ",
        "from ",
        "use ",
        "pub use ",
        "pub(crate) use ",
        "export {",
        "export *",
        "require ",
    ];
    trimmed == "*" || PREFIXES.iter().any(|prefix| trimmed.starts_with(prefix))
}

fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

/// Whether `line` uses `name` as a whole word somewhere other than right
/// after a keyword that defines it (`fn name`, `function name`, ...).
fn uses_name(line: &str, name: &str) -> bool {
    const DEFINING: [&str; 13] = [
        "fn",
        "function",
        "def",
        "func",
        "fun",
        "sub",
        "class",
        "struct",
        "enum",
        "trait",
        "interface",
        "type",
        "macro_rules!",
    ];
    line.match_indices(name).any(|(start, _)| {
        let before = &line[..start];
        let after = &line[start + name.len()..];
        let whole_word = !before.chars().next_back().is_some_and(is_identifier_char)
            && !after.chars().next().is_some_and(is_identifier_char);
        let defined_here = before
            .trim_end()
            .rsplit(|character: char| character.is_whitespace())
            .next()
            .is_some_and(|word| DEFINING.contains(&word));
        whole_word && !defined_here
    })
}

/// True when a symbol named `name` starts on `line` (1-based): the line is
/// the name's definition, not a use of it.
fn defines_name_at(symbols: Option<&[Symbol]>, name: &str, line: u32) -> bool {
    symbols
        .unwrap_or_default()
        .iter()
        .any(|symbol| symbol.name == name && symbol.range.start_line.saturating_add(1) == line)
}

/// The innermost symbol whose range holds `line` (1-based).
fn enclosing_symbol(symbols: Option<&[Symbol]>, line: u32) -> String {
    let line = line.saturating_sub(1);
    symbols
        .unwrap_or_default()
        .iter()
        .filter(|symbol| !matches!(symbol.kind, SymbolKind::Heading | SymbolKind::FileSummary))
        .filter(|symbol| symbol.range.start_line <= line && line <= symbol.range.end_line)
        .max_by_key(|symbol| {
            (
                symbol.range.start_line,
                std::cmp::Reverse(symbol.range.end_line),
            )
        })
        .map(|symbol| symbol.name.clone())
        .unwrap_or_else(|| "<top-level>".to_string())
}

/// Make a borrowed-graph answer account for the files that differ from the
/// borrowed graph, which the graph cannot see.
///
/// `callers` and `impact` answers gain the matches `search` finds for the
/// target's name in those files, marked name-matched. Any answer that is
/// still empty, or whose search stopped at a bound, gets a
/// `borrowed_coverage` note naming the files the graph does not reflect and
/// suggesting grep, so it never reads as a definitive zero. A borrowed graph
/// with no differing file leaves the answer as it was.
fn cover_differing_files(
    data: &mut Map<String, Value>,
    borrowed: &BorrowedCallgraph,
    op: BorrowedOp,
    query: &CoverageQuery,
    search: impl FnOnce(&[String], &str) -> NameSearch,
) {
    let (files, list_complete) = match &borrowed.differing_files {
        Some(differing) => (differing.files.as_slice(), differing.complete),
        None => (&[][..], false),
    };
    if files.is_empty() && list_complete {
        return;
    }
    let name = query.name.as_deref();
    let search = match (op, name) {
        (BorrowedOp::Callers | BorrowedOp::Impact, Some(name)) if !files.is_empty() => {
            Some(search(files, name))
        }
        _ => None,
    };
    let found = match &search {
        Some(search) if op == BorrowedOp::Callers => {
            merge_callers(data, &search.matches, query.include_tests)
        }
        Some(search) => merge_impact(data, &search.matches, query.include_tests),
        None => Found::default(),
    };
    let empty = answer_is_empty(op, data);
    let Some(message) = coverage_message(
        op,
        name,
        files,
        list_complete,
        search.as_ref(),
        &found,
        empty,
    ) else {
        return;
    };
    data.insert(
        "borrowed_coverage".to_string(),
        json!({
            "message": message,
            "name": name,
            "differing_files": files.len(),
            "differing_files_complete": list_complete,
            "name_matched": found.shown + found.hidden,
            "files_searched": search.as_ref().map_or(0, |search| search.searched),
            "files_not_searched": search.as_ref().map_or(0, |search| search.not_searched.len()),
            "search_limit": search
                .as_ref()
                .and_then(|search| search.stopped_by)
                .map(SearchStop::label),
        }),
    );
}

/// Name matches merged into an answer: listed, and hidden as test files.
#[derive(Debug, Default, PartialEq, Eq)]
struct Found {
    shown: usize,
    hidden: usize,
}

/// Split matches into ones to list and ones hidden as tests, dropping any
/// line the answer already lists for that file.
fn new_matches<'a>(
    matches: &'a [NameMatch],
    listed: &HashSet<(String, u64)>,
    include_tests: bool,
) -> (Vec<&'a NameMatch>, usize) {
    let mut shown = Vec::new();
    let mut hidden = 0;
    for found in matches {
        if listed.contains(&(found.file.clone(), u64::from(found.line))) {
            continue;
        }
        if found.in_test && !include_tests {
            hidden += 1;
        } else {
            shown.push(found);
        }
    }
    (shown, hidden)
}

fn add_to_count(data: &mut Map<String, Value>, field: &str, added: usize) {
    if added == 0 {
        return;
    }
    let current = data.get(field).and_then(Value::as_u64).unwrap_or(0);
    data.insert(field.to_string(), json!(current + added as u64));
}

/// Count `added` more listed entries in a list envelope, when the answer has
/// one.
fn add_to_envelope(data: &mut Map<String, Value>, field: &str, added: usize) {
    if added == 0 {
        return;
    }
    let Some(envelope) = data.get_mut(field).and_then(Value::as_object_mut) else {
        return;
    };
    add_to_count(envelope, "shown", added);
    if let Some(total) = envelope.get_mut("total").and_then(Value::as_object_mut) {
        add_to_count(total, "value", added);
    }
}

/// Add name matches to a `callers` answer: grouped by file, marked
/// name-matched like the graph's own name-resolved callers.
fn merge_callers(
    data: &mut Map<String, Value>,
    matches: &[NameMatch],
    include_tests: bool,
) -> Found {
    let mut groups: Vec<Value> = data
        .get("callers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let listed: HashSet<(String, u64)> = groups
        .iter()
        .flat_map(|group| {
            let file = group["file"].as_str().unwrap_or_default().to_string();
            group["callers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(move |caller| Some((file.clone(), caller["line"].as_u64()?)))
        })
        .collect();
    let (shown, hidden) = new_matches(matches, &listed, include_tests);
    for found in &shown {
        let entry = json!({
            "symbol": found.symbol,
            "line": found.line,
            "approximate": true,
            "resolved_by": "name_match",
        });
        let group = groups
            .iter_mut()
            .find(|group| group["file"].as_str() == Some(found.file.as_str()));
        match group.and_then(|group| group["callers"].as_array_mut()) {
            Some(callers) => callers.push(entry),
            None => groups.push(json!({ "file": found.file, "callers": [entry] })),
        }
    }
    groups.sort_by(|left, right| left["file"].as_str().cmp(&right["file"].as_str()));
    if !shown.is_empty() {
        data.insert("callers".to_string(), Value::Array(groups));
    }
    // `total_callers` counts callers in tests whether or not they are hidden;
    // the envelope counts only the listed ones.
    add_to_count(data, "total_callers", shown.len() + hidden);
    add_to_count(data, "hidden_test_callers", hidden);
    add_to_envelope(data, "callers_list_envelope", shown.len());
    Found {
        shown: shown.len(),
        hidden,
    }
}

/// Add name matches to an `impact` answer as affected call sites, marked
/// name-matched, with the matching line as the call expression.
fn merge_impact(
    data: &mut Map<String, Value>,
    matches: &[NameMatch],
    include_tests: bool,
) -> Found {
    let mut callers: Vec<Value> = data
        .get("callers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let listed: HashSet<(String, u64)> = callers
        .iter()
        .filter_map(|caller| {
            Some((
                caller["caller_file"].as_str()?.to_string(),
                caller["line"].as_u64()?,
            ))
        })
        .collect();
    let listed_files: HashSet<&str> = listed.iter().map(|(file, _)| file.as_str()).collect();
    let (shown, hidden) = new_matches(matches, &listed, include_tests);
    let new_files = shown
        .iter()
        .map(|found| found.file.as_str())
        .filter(|file| !listed_files.contains(file))
        .collect::<HashSet<_>>()
        .len();
    for found in &shown {
        callers.push(json!({
            "caller_symbol": found.symbol,
            "caller_file": found.file,
            "line": found.line,
            "is_entry_point": false,
            "call_expression": found.text,
            "parameters": [],
            "approximate": true,
            "resolved_by": "name_match",
        }));
    }
    callers.sort_by(|left, right| {
        left["caller_file"]
            .as_str()
            .cmp(&right["caller_file"].as_str())
            .then(left["line"].as_u64().cmp(&right["line"].as_u64()))
    });
    if !shown.is_empty() {
        data.insert("callers".to_string(), Value::Array(callers));
    }
    add_to_count(data, "total_affected", shown.len() + hidden);
    add_to_count(data, "hidden_test_callers", hidden);
    add_to_count(data, "affected_files", new_files);
    add_to_envelope(data, "sites_list_envelope", shown.len());
    Found {
        shown: shown.len(),
        hidden,
    }
}

/// Whether the answer lists nothing: no callers, no callees, no path.
fn answer_is_empty(op: BorrowedOp, data: &Map<String, Value>) -> bool {
    let list = match op {
        BorrowedOp::Callers | BorrowedOp::Impact => "callers",
        BorrowedOp::CallTree => "children",
        BorrowedOp::TraceTo => "paths",
        BorrowedOp::TraceToSymbol => "path",
        BorrowedOp::TraceData => "hops",
    };
    data.get(list)
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
}

fn plural(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

/// How many paths a coverage note names before saying how many more there
/// are.
const SAMPLE_SHOWN: usize = 3;

/// Up to [`SAMPLE_SHOWN`] paths, then how many more.
fn sample(files: &[String]) -> String {
    let mut text = files
        .iter()
        .take(SAMPLE_SHOWN)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if files.len() > SAMPLE_SHOWN {
        text.push_str(&format!(" and {} more", files.len() - SAMPLE_SHOWN));
    }
    text
}

/// The one-line coverage note for an answer, or `None` when the answer needs
/// none: it lists something, and every differing file is known and was fully
/// searched or cannot hold what the operation reports.
fn coverage_message(
    op: BorrowedOp,
    name: Option<&str>,
    files: &[String],
    list_complete: bool,
    search: Option<&NameSearch>,
    found: &Found,
    empty: bool,
) -> Option<String> {
    let (singular, plural_noun) = match op {
        BorrowedOp::Impact => ("call site", "call sites"),
        _ => ("caller", "callers"),
    };
    let grep = match name {
        Some(name) => format!("grep for `{name}`"),
        None => "grep for the symbol".to_string(),
    };
    let files_phrase = if list_complete {
        plural(files.len(), "file", "files")
    } else if files.is_empty() {
        "an unknown number of files".to_string()
    } else {
        format!("at least {}", plural(files.len(), "file", "files"))
    };
    let not_reflected = format!("{files_phrase} here that the borrowed graph does not reflect");
    let named = if files.is_empty() {
        String::new()
    } else {
        format!(" ({})", sample(files))
    };
    let searched_for = name.unwrap_or("the symbol");

    if let Some(search) = search.filter(|search| search.partial()) {
        let missed = &search.not_searched;
        let stop = match search.stopped_by {
            Some(stop) => {
                let limit = match stop {
                    SearchStop::Files => format!("its limit of {} files", search.bounds.max_files),
                    SearchStop::Deadline => format!(
                        "its time limit of {} ms",
                        search.bounds.deadline.as_millis()
                    ),
                    SearchStop::Matches => {
                        format!("its limit of {} matches", search.bounds.max_matches)
                    }
                };
                if missed.is_empty() {
                    format!("the search stopped at {limit}")
                } else {
                    format!(
                        "the search stopped at {limit} with {} not searched ({})",
                        plural(missed.len(), "file", "files"),
                        sample(missed)
                    )
                }
            }
            None => format!(
                "{} too large or unreadable {} not searched ({})",
                plural(missed.len(), "file", "files"),
                if missed.len() == 1 { "was" } else { "were" },
                sample(missed)
            ),
        };
        let total = found.shown + found.hidden;
        let opening = if total > 0 {
            format!(
                "found {} of `{searched_for}` by name, marked ~, in {not_reflected}{}",
                plural(total, singular, plural_noun),
                hidden_clause(found.hidden)
            )
        } else {
            format!("the borrowed graph does not reflect {files_phrase} here{named}")
        };
        return Some(format!(
            "callgraph: {opening}; {stop}, so {grep} to cover the rest"
        ));
    }

    let total = found.shown + found.hidden;
    if total > 0 {
        return Some(format!(
            "callgraph: found {} of `{searched_for}` by name, marked ~, in {not_reflected}{}",
            plural(total, singular, plural_noun),
            hidden_clause(found.hidden)
        ));
    }
    if !empty {
        return None;
    }
    let message = match search {
        Some(_) if list_complete => format!(
            "callgraph: the borrowed graph does not reflect {files_phrase} here{named}; searching them for `{searched_for}` found no {plural_noun}, but {grep} to confirm"
        ),
        Some(_) => format!(
            "callgraph: the borrowed graph does not reflect {files_phrase} here{named}; searching the known ones for `{searched_for}` found no {plural_noun}, but more files may differ, so {grep} to check them"
        ),
        None => {
            let missing = match op {
                BorrowedOp::Callers | BorrowedOp::Impact => {
                    format!("{plural_noun} there are missing from this answer")
                }
                _ => "a path through them would not appear in this answer".to_string(),
            };
            format!(
                "callgraph: the borrowed graph does not reflect {files_phrase} here{named}, so {missing}; {grep} to check them"
            )
        }
    };
    Some(message)
}

fn hidden_clause(hidden: usize) -> String {
    if hidden == 0 {
        String::new()
    } else {
        format!(" ({hidden} in tests, hidden — includeTests: true shows them)")
    }
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
            edits_in: None,
            differing_files: None,
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
            "callgraph: borrowed from /work/owner at aaaaaaa; this checkout is at bbbbbbb with 3 files that differ from the borrowed graph, so callers and line numbers may not match this tree"
        );
        let unknown = BorrowedCallgraph {
            owner_checkout: None,
            owner_head: None,
            checkout_head: Some(B.to_string()),
            changed_files: None,
            edits_in: None,
            differing_files: None,
        }
        .not_found_hint();
        assert_eq!(
            unknown,
            "the callgraph is borrowed from another checkout at an unknown commit; this checkout is at bbbbbbb with an unknown number of files that differ from the borrowed graph, so the symbol may exist in this checkout but not in the borrowed graph; use grep or aft_zoom on this checkout to confirm"
        );
        assert!(!line.contains('\n') && !unknown.contains('\n'));
        assert!(borrowed(Some(A), Some(B), Some(1))
            .summary_line()
            .contains("with 1 file that differs from the borrowed graph,"));
    }

    #[test]
    fn disclosure_names_the_checkout_holding_the_edits_when_known() {
        let located = |changed_files, edits_in| BorrowedCallgraph {
            edits_in,
            ..borrowed(Some(A), Some(A), changed_files)
        };
        assert_eq!(
            located(Some(3), Some(EditsIn::BorrowedCheckout)).summary_line(),
            "callgraph: borrowed from /work/owner at aaaaaaa; this checkout is at aaaaaaa with 3 files that differ from the borrowed graph (edits in the borrowed checkout), so callers and line numbers may not match this tree"
        );
        assert!(located(Some(2), Some(EditsIn::ThisCheckout))
            .not_found_hint()
            .contains("with 2 files that differ from the borrowed graph (edits in this checkout), so the symbol"));
        assert!(located(Some(2), Some(EditsIn::BothCheckouts))
            .summary_line()
            .contains("(edits in both checkouts), so callers"));
        // Without a count there is nothing to locate.
        assert!(!located(None, Some(EditsIn::ThisCheckout))
            .summary_line()
            .contains("(edits"));
        assert_eq!(
            located(Some(3), Some(EditsIn::BorrowedCheckout)).to_json()["edits_in"],
            json!("borrowed_checkout")
        );
    }

    fn identity(content: &str) -> FileIdentity {
        (
            content.len() as u64,
            cache_freshness::hash_bytes(content.as_bytes()),
        )
    }

    fn unhashed(content: &str) -> FileIdentity {
        (content.len() as u64, cache_freshness::zero_hash())
    }

    fn files(entries: &[(&str, FileIdentity)]) -> HashMap<String, FileIdentity> {
        entries
            .iter()
            .map(|(path, identity)| (path.to_string(), *identity))
            .collect()
    }

    /// A checkout and an owner directory holding the given files.
    fn trees(
        checkout_files: &[(&str, &str)],
        owner_files: &[(&str, &str)],
    ) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let checkout = temp.path().join("checkout");
        let owner = temp.path().join("owner");
        for (root, entries) in [(&checkout, checkout_files), (&owner, owner_files)] {
            fs::create_dir_all(root).unwrap();
            for (path, content) in entries {
                let path = root.join(path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, content).unwrap();
            }
        }
        (temp, checkout, owner)
    }

    #[test]
    fn identical_files_never_count_even_when_one_side_never_hashed_them() {
        let big = "x".repeat(64);
        let (_temp, checkout, owner) = trees(
            &[
                ("src/a.ts", "a"),
                ("src/big.ts", &big),
                ("src/huge.ts", "huge"),
            ],
            &[],
        );
        // The graph hashed big.ts but the search index held it unindexed
        // (too large for it), so its content is read from disk; huge.ts is
        // past the graph's hash cap, so only sizes are compared.
        let graph = files(&[
            ("src/a.ts", identity("a")),
            ("src/big.ts", identity(&big)),
            ("src/huge.ts", unhashed("huge")),
        ]);
        let here = files(&[
            ("src/a.ts", identity("a")),
            ("src/big.ts", unhashed(&big)),
            ("src/huge.ts", identity("huge")),
            // Files the graph's walk never includes: no supported source
            // type, a hidden path, the `.git` file that points a linked
            // worktree at its git directory, or an always-skipped directory.
            ("assets/blob.bin", unhashed("\0\x01")),
            ("notes.txt", identity("notes")),
            (".github/ci.ts", identity("ci")),
            (".git", identity("gitdir: elsewhere")),
            ("dist/out.ts", identity("built")),
        ]);
        assert_eq!(
            compare_with_graph(&graph, here, &checkout, Some(&owner), 8),
            known(&[])
        );
    }

    #[test]
    fn edited_missing_and_new_source_files_each_count_once() {
        let (_temp, checkout, owner) = trees(
            &[
                ("src/edited.ts", "new body"),
                ("src/unhashed.ts", "other!"),
                ("src/unindexed.ts", "on disk only"),
                ("src/added.ts", "added here"),
                ("src/rejected.ts", "same in both"),
            ],
            &[("src/rejected.ts", "same in both")],
        );
        let graph = files(&[
            ("src/edited.ts", identity("old body")),
            ("src/unhashed.ts", identity("origin")),
            ("src/unindexed.ts", identity("on disk only")),
            ("src/gone.ts", identity("deleted here")),
            ("src/gone_unhashed.ts", unhashed("too big")),
        ]);
        let here = files(&[
            ("src/edited.ts", identity("new body")),
            // Same size as the graph's copy, different bytes on disk.
            ("src/unhashed.ts", unhashed("other!")),
            ("src/added.ts", identity("added here")),
            // The graph lacks it, but the borrowed checkout holds the same
            // content: the graph leaves it out there just as it would here.
            ("src/rejected.ts", identity("same in both")),
        ]);
        // unindexed.ts is in the graph but has no record in this checkout's
        // index; its file on disk is hashed instead and matches the graph.
        assert_eq!(
            compare_with_graph(&graph, here, &checkout, Some(&owner), 8),
            known(&[
                "src/added.ts",
                "src/edited.ts",
                "src/gone.ts",
                "src/gone_unhashed.ts",
                "src/unhashed.ts",
            ])
        );
    }

    #[test]
    fn a_new_file_counts_when_the_owner_is_unknown_and_reading_past_the_budget_is_incomplete() {
        let (_temp, checkout, owner) = trees(
            &[("src/a.ts", "aa"), ("src/b.ts", "bb")],
            &[("src/a.ts", "aa")],
        );
        let here = files(&[("src/a.ts", identity("aa"))]);
        assert_eq!(
            compare_with_graph(&HashMap::new(), here, &checkout, None, 8),
            known(&["src/a.ts"])
        );
        let graph = files(&[("src/a.ts", identity("aa")), ("src/b.ts", identity("bb"))]);
        let here = files(&[("src/a.ts", unhashed("aa")), ("src/b.ts", unhashed("bb"))]);
        assert_eq!(
            compare_with_graph(&graph, here.clone(), &checkout, Some(&owner), 2),
            known(&[])
        );
        // Past the budget the files found so far are kept, but the list is
        // marked incomplete so that no count is reported from it.
        assert_eq!(
            compare_with_graph(&graph, here, &checkout, Some(&owner), 1),
            DifferingFiles {
                files: Vec::new(),
                complete: false
            }
        );
        let graph = files(&[("src/a.ts", identity("zz")), ("src/b.ts", identity("bb"))]);
        let here = files(&[("src/a.ts", identity("aa")), ("src/b.ts", unhashed("bb"))]);
        assert_eq!(
            compare_with_graph(&graph, here, &checkout, Some(&owner), 0),
            DifferingFiles {
                files: vec!["src/a.ts".to_string()],
                complete: false
            }
        );
    }

    fn known(paths: &[&str]) -> DifferingFiles {
        DifferingFiles {
            files: paths.iter().map(|path| path.to_string()).collect(),
            complete: true,
        }
    }

    #[test]
    fn only_whole_word_uses_outside_definitions_comments_and_imports_match() {
        let text = "import { target } from './t';\n\
                    // target() is called below\n\
                    export function target() {}\n\
                    const x = target();\n\
                    const y = targeted() + my_target();\n\
                    run(target);\n\
                    fn target() {}\n\
                    use crate::target;\n";
        let lines: Vec<u32> = lines_using_name(text, "target")
            .into_iter()
            .map(|(line, _)| line)
            .collect();
        assert_eq!(lines, vec![4, 6]);
        assert_eq!(bare_name("Type::method"), "method");
        assert_eq!(bare_name("Class.method"), "method");
        assert_eq!(bare_name("plain"), "plain");
    }

    fn search_tree(files: &[(&str, &str)]) -> (tempfile::TempDir, Vec<String>) {
        let temp = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let path = temp.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        let mut names: Vec<String> = files.iter().map(|(path, _)| path.to_string()).collect();
        names.sort();
        (temp, names)
    }

    #[test]
    fn the_search_names_the_enclosing_symbol_and_stops_at_each_bound() {
        let provider = crate::parser::TreeSitterProvider::new();
        let (temp, mut files) = search_tree(&[
            (
                "src/a.ts",
                "export function a() {\n  return target();\n}\ntarget();\n",
            ),
            ("src/b.ts", "export function b() { return target(); }\n"),
            ("src/c.test.ts", "it('x', () => target());\n"),
        ]);
        // A file the graph has but this checkout deleted is skipped silently.
        files.push("src/deleted.ts".to_string());
        // The production 250 ms deadline is real wall time, and a loaded CI
        // runner can spend it parsing the first file. This case is about the
        // file and match bounds, so give the clock room; the deadline bound
        // has its own case below with a zero deadline.
        let roomy = SearchBounds {
            deadline: Duration::from_secs(60),
            ..SearchBounds::DEFAULT
        };
        let search = search_differing_files(&provider, temp.path(), &files, "target", &roomy);
        let found: Vec<(&str, u32, &str, bool)> = search
            .matches
            .iter()
            .map(|m| (m.file.as_str(), m.line, m.symbol.as_str(), m.in_test))
            .collect();
        assert_eq!(
            found,
            vec![
                ("src/a.ts", 2, "a", false),
                ("src/a.ts", 4, "<top-level>", false),
                ("src/b.ts", 1, "b", false),
                ("src/c.test.ts", 1, "<top-level>", true),
            ]
        );
        assert_eq!(search.searched, 3);
        assert!(!search.partial(), "{search:?}");

        let few_files = SearchBounds {
            max_files: 1,
            ..roomy
        };
        let search = search_differing_files(&provider, temp.path(), &files, "target", &few_files);
        assert_eq!(search.stopped_by, Some(SearchStop::Files));
        assert_eq!(search.searched, 1);
        assert_eq!(
            search.not_searched,
            vec!["src/b.ts", "src/c.test.ts", "src/deleted.ts"]
        );

        let few_matches = SearchBounds {
            max_matches: 1,
            ..roomy
        };
        let search = search_differing_files(&provider, temp.path(), &files, "target", &few_matches);
        assert_eq!(search.stopped_by, Some(SearchStop::Matches));
        assert_eq!(search.matches.len(), 1);
        assert_eq!(
            search.not_searched.first().map(String::as_str),
            Some("src/b.ts")
        );

        let no_time = SearchBounds {
            deadline: Duration::ZERO,
            ..roomy
        };
        let search = search_differing_files(&provider, temp.path(), &files, "target", &no_time);
        assert_eq!(search.stopped_by, Some(SearchStop::Deadline));
        assert_eq!(search.searched, 0);

        let small_files = SearchBounds {
            max_file_bytes: 45,
            ..roomy
        };
        let search = search_differing_files(&provider, temp.path(), &files, "target", &small_files);
        assert_eq!(search.stopped_by, None);
        assert_eq!(search.not_searched, vec!["src/a.ts"]);
        assert!(search.partial());
    }

    fn borrowed_with(differing: Option<DifferingFiles>) -> BorrowedCallgraph {
        BorrowedCallgraph {
            changed_files: differing
                .as_ref()
                .filter(|differing| differing.complete)
                .map(|differing| differing.files.len()),
            differing_files: differing,
            ..borrowed(Some(A), Some(B), None)
        }
    }

    fn query(name: &str) -> CoverageQuery {
        CoverageQuery {
            name: Some(name.to_string()),
            include_tests: false,
        }
    }

    fn name_match(file: &str, line: u32, symbol: &str, in_test: bool) -> NameMatch {
        NameMatch {
            file: file.to_string(),
            line,
            symbol: symbol.to_string(),
            text: format!("{symbol}();"),
            in_test,
        }
    }

    fn finished(matches: Vec<NameMatch>, searched: usize) -> NameSearch {
        NameSearch {
            matches,
            searched,
            not_searched: Vec::new(),
            stopped_by: None,
            bounds: SearchBounds::DEFAULT,
        }
    }

    #[test]
    fn matches_merge_into_callers_without_repeating_listed_lines() {
        let mut data = json!({
            "symbol": "target",
            "callers": [{ "file": "src/z.ts", "callers": [{ "symbol": "z", "line": 3 }] }],
            "total_callers": 21,
            "callers_list_envelope": {
                "shown": 15, "total": { "kind": "exact", "value": 20 },
                "unit": "items", "reason": "cap", "causes": ["cap"], "narrow": []
            }
        });
        let data = data.as_object_mut().unwrap();
        let borrowed = borrowed_with(Some(known(&["src/a.ts", "src/t.test.ts", "src/z.ts"])));
        cover_differing_files(
            data,
            &borrowed,
            BorrowedOp::Callers,
            &query("target"),
            |files, name| {
                assert_eq!((files.len(), name), (3, "target"));
                finished(
                    vec![
                        name_match("src/a.ts", 7, "a", false),
                        name_match("src/t.test.ts", 1, "<top-level>", true),
                        name_match("src/z.ts", 3, "z", false),
                    ],
                    3,
                )
            },
        );
        let groups: Vec<&str> = data["callers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|group| group["file"].as_str().unwrap())
            .collect();
        assert_eq!(groups, vec!["src/a.ts", "src/z.ts"]);
        assert_eq!(data["callers"][1]["callers"].as_array().unwrap().len(), 1);
        assert_eq!(data["total_callers"], json!(23));
        assert_eq!(data["hidden_test_callers"], json!(1));
        assert_eq!(data["callers_list_envelope"]["shown"], json!(16));
        assert_eq!(data["callers_list_envelope"]["total"]["value"], json!(21));
        assert_eq!(
            data["borrowed_coverage"]["message"],
            json!("callgraph: found 2 callers of `target` by name, marked ~, in 3 files here that the borrowed graph does not reflect (1 in tests, hidden — includeTests: true shows them)")
        );
    }

    #[test]
    fn an_empty_answer_names_the_files_the_graph_cannot_see() {
        let files = known(&["src/a.ts", "src/b.ts", "src/c.ts", "src/d.ts"]);
        let message = |op, differing: Option<DifferingFiles>, searched: bool| {
            let mut data = json!({ "symbol": "target", "callers": [], "paths": [], "path": null });
            let data = data.as_object_mut().unwrap();
            cover_differing_files(
                data,
                &borrowed_with(differing),
                op,
                &query("target"),
                |_, _| {
                    assert!(searched, "{op:?} searched differing files");
                    finished(Vec::new(), 4)
                },
            );
            data.get("borrowed_coverage")
                .map(|coverage| coverage["message"].clone())
        };
        assert_eq!(
            message(BorrowedOp::Callers, Some(files.clone()), true),
            Some(json!("callgraph: the borrowed graph does not reflect 4 files here (src/a.ts, src/b.ts, src/c.ts and 1 more); searching them for `target` found no callers, but grep for `target` to confirm"))
        );
        assert_eq!(
            message(BorrowedOp::TraceTo, Some(files.clone()), false),
            Some(json!("callgraph: the borrowed graph does not reflect 4 files here (src/a.ts, src/b.ts, src/c.ts and 1 more), so a path through them would not appear in this answer; grep for `target` to check them"))
        );
        assert_eq!(
            message(
                BorrowedOp::TraceToSymbol,
                Some(DifferingFiles { complete: false, ..files.clone() }),
                false
            ),
            Some(json!("callgraph: the borrowed graph does not reflect at least 4 files here (src/a.ts, src/b.ts, src/c.ts and 1 more), so a path through them would not appear in this answer; grep for `target` to check them"))
        );
        assert_eq!(
            message(BorrowedOp::Impact, None, false),
            Some(json!("callgraph: the borrowed graph does not reflect an unknown number of files here, so call sites there are missing from this answer; grep for `target` to check them"))
        );
        // No differing file: the answer is left exactly as it was.
        assert_eq!(message(BorrowedOp::Callers, Some(known(&[])), false), None);
        assert_eq!(message(BorrowedOp::TraceTo, Some(known(&[])), false), None);
    }

    #[test]
    fn a_listed_answer_with_every_file_searched_gets_no_note() {
        let mut data = json!({ "symbol": "target", "paths": [{ "hops": [] }] });
        let before = data.clone();
        let data_map = data.as_object_mut().unwrap();
        cover_differing_files(
            data_map,
            &borrowed_with(Some(known(&["src/a.ts"]))),
            BorrowedOp::TraceTo,
            &query("target"),
            |_, _| unreachable!("trace answers are not searched"),
        );
        assert_eq!(data, before);
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
