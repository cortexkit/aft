//! `views.callgraph_copies`: how many checkouts of one repository keep their
//! own call graph, besides the main checkout.
//!
//! A checkout's call-graph view costs a base database for its commit plus a
//! member database for its edits. Where the filesystem clones files the member
//! shares unchanged pages with its base, but a commit the repository has no
//! base for, or one edit to a file most of the code depends on, still costs
//! most of a full graph (1.3 to 2 GB for AFT's own repository). So the number
//! of checkouts that build one is limited. A checkout over the limit reads the
//! main checkout's published graph instead, and its answers say which of its
//! files differ (`commands::callgraph_borrowed`).
//!
//! The limit counts the repository's active `<scope>.callgraph` members other
//! than the main checkout's. When it is reached, the least recently used
//! member nobody holds (no live session, pin, reader or open handle) is
//! evicted to make room; when every member is held, the new checkout borrows.
//! A checkout whose files equal the graph it would borrow borrows without
//! counting, since a copy of its own would hold the same graph. Admission and
//! registration are separate steps, so checkouts starting at the same moment
//! can overshoot the limit by their number; later admissions evict back down.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::callgraph_store::ReadonlyCallGraphStore;

/// Whether a checkout builds its own call-graph view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Build and read this checkout's own call-graph view.
    Own,
    /// Read the published call-graph view of `root` (the main checkout, or
    /// the checkout standing in for it while the main one has none).
    Borrow { root: PathBuf, scope: String },
}

const SUFFIX: &str = ".callgraph";

/// Members examined when looking for one to evict.
const EVICTION_CANDIDATES: usize = 64;

/// Files read from disk while deciding whether a checkout equals the graph it
/// would borrow (files the checkout's index never hashed). Past it the
/// checkout is treated as different.
const MATCH_DISK_READS: usize = 64;

/// The longest admission spends waiting for the storage lock and evicting.
const EVICTION_BUDGET: Duration = Duration::from_secs(5);

struct Member {
    scope: String,
    root: Option<PathBuf>,
    registered_at_ms: i64,
}

/// Decides whether the checkout at `root` builds its own call-graph view
/// under a limit of `copies`. `identities` gives this checkout's current file
/// identities (size and content hash by relative path) when they are known,
/// so a checkout identical to the graph it would borrow is not counted.
pub(crate) fn admit(
    storage: &Path,
    family: &str,
    policy: crate::blob_store::v2::TrigramPolicy,
    root: &Path,
    copies: u32,
    identities: impl FnOnce() -> Option<HashMap<String, (u64, blake3::Hash)>>,
) -> Result<Admission, String> {
    let Some(main) = super::read::main_checkout_root(root) else {
        return Ok(Admission::Own);
    };
    if main == root {
        return Ok(Admission::Own);
    }
    let own = super::callgraph::view_scope(root);
    let members = active_members(storage, family)?;
    if members.iter().any(|member| member.scope == own) {
        return Ok(Admission::Own);
    }
    let main_scope = super::callgraph::view_scope(&main);
    // While the main checkout has no call graph, the oldest checkout that has
    // one stands in for it, so borrowers still have a graph to read.
    let Some(target) = members
        .iter()
        .find(|member| member.scope == main_scope)
        .or_else(|| {
            members
                .iter()
                .min_by(|a, b| (a.registered_at_ms, &a.scope).cmp(&(b.registered_at_ms, &b.scope)))
        })
    else {
        return Ok(Admission::Own);
    };
    let borrow = Admission::Borrow {
        root: target.root.clone().unwrap_or_else(|| main.clone()),
        scope: target.scope.clone(),
    };
    if copies == 0 || matches_target(storage, family, policy, &target.scope, root, identities) {
        return Ok(borrow);
    }
    let owned = members.iter().filter(|m| m.scope != target.scope).count();
    if owned < copies as usize {
        return Ok(Admission::Own);
    }
    // At the limit: make room by evicting copies nobody is using.
    let needed = owned + 1 - copies as usize;
    let deadline = Instant::now() + EVICTION_BUDGET;
    let lock = crate::blob_store::v3::ReservationLock::acquire_until(storage, deadline)
        .map_err(|error| error.to_string())?;
    let mut registry = crate::blob_store::v3::open_base_refs_registry(storage, family, &lock, None)
        .map_err(|error| error.to_string())?;
    for _ in 0..needed {
        let evicted = crate::storage_retention::evict_least_recent_v3_member(
            storage,
            family,
            &mut registry,
            SUFFIX,
            &[target.scope.as_str(), own.as_str()],
            EVICTION_CANDIDATES,
            deadline,
        )
        .map_err(|error| error.to_string())?;
        match evicted {
            Some(scope) => crate::slog_info!(
                "views callgraph copy evicted scope={scope} for={own} limit={copies}"
            ),
            None => return Ok(borrow),
        }
    }
    Ok(Admission::Own)
}

/// True when the checkout's known files equal the target's published graph,
/// so borrowing it answers exactly as a copy of its own would.
fn matches_target(
    storage: &Path,
    family: &str,
    policy: crate::blob_store::v2::TrigramPolicy,
    target_scope: &str,
    root: &Path,
    identities: impl FnOnce() -> Option<HashMap<String, (u64, blake3::Hash)>>,
) -> bool {
    let Ok((store, _)) = open_published(storage, family, policy, target_scope, root) else {
        return false;
    };
    let (Some(checkout), Ok(graph)) = (identities(), store.file_identities()) else {
        return false;
    };
    let differing = crate::commands::callgraph_borrowed::compare_with_graph(
        &graph,
        checkout,
        root,
        None,
        MATCH_DISK_READS,
    );
    differing.complete && differing.files.is_empty()
}

/// Opens the published call graph of the member `scope`, bound to `root` so
/// answers name `root`'s files. `Err((reason, busy))`; `busy` means the graph
/// is not published yet or is being replaced, and a later call can succeed.
pub(crate) fn open_published(
    storage: &Path,
    family: &str,
    policy: crate::blob_store::v2::TrigramPolicy,
    scope: &str,
    root: &Path,
) -> Result<(ReadonlyCallGraphStore, String), (String, bool)> {
    // The producers a checkout's call-graph view publishes under
    // (`CallgraphView::new_with_import`).
    let producers = super::manifest_v2::Producers {
        trigram: policy.fingerprint_hex(),
        semantic: None,
        callgraph: super::callgraph::PRODUCER.into(),
    };
    let generation = match super::base_refs::open_foreign_v3(storage, family, scope, &producers) {
        Ok(Some(generation)) => generation,
        Ok(None) => return Err(("the main checkout's call graph is not published yet".into(), true)),
        Err(super::base_refs::BaseRefError::Unavailable(reason)) => return Err((reason, true)),
        Err(error) => return Err((error.to_string(), false)),
    };
    let Some(route) = generation.v3_route().cloned() else {
        return Err(("call graph view has no v3 route".into(), false));
    };
    let name = generation.name().to_owned();
    match ReadonlyCallGraphStore::open_pinned_derived(
        root.to_path_buf(),
        family.to_owned(),
        route.member_dir(),
        &name,
    ) {
        Ok(store) => Ok((store.retain_generation(generation), name)),
        Err(crate::callgraph_store::CallGraphStoreError::Unavailable(reason)) => Err((reason, true)),
        Err(error) => Err((error.to_string(), false)),
    }
}

/// The current published generation of the member `scope`, read without a
/// lock, to tell whether a cached borrowed reader is still current.
pub(crate) fn published_generation(storage: &Path, family: &str, scope: &str) -> Option<String> {
    let dir = crate::blob_store::v3::view_dir(storage, family, scope).ok()?;
    super::ViewStore::existing_dir(dir)?
        .current_generation_read_only()
        .ok()
        .flatten()
}

fn active_members(storage: &Path, family: &str) -> Result<Vec<Member>, String> {
    let registry = crate::blob_store::v3::family_dir(storage, family)
        .map_err(|error| error.to_string())?
        .join("members.sqlite");
    if !registry.try_exists().map_err(|error| error.to_string())? {
        return Ok(Vec::new());
    }
    let reader = crate::blob_store::v3::LedgerReader::open_existing(storage, family)
        .map_err(|error| error.to_string())?;
    reader
        .read(|connection| {
            let mut statement = connection.prepare(
                "SELECT scope,root,registered_at_ms FROM members
                 WHERE kind='view' AND state='active' AND scope LIKE ?1",
            )?;
            let rows = statement
                .query_map([format!("%{SUFFIX}")], |row| {
                    let root: Option<Vec<u8>> = row.get(1)?;
                    Ok(Member {
                        scope: row.get(0)?,
                        root: root.as_deref().and_then(decode_root),
                        registered_at_ms: row.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .map_err(|error| error.to_string())
}

fn decode_root(root: &[u8]) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(root)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(root).ok().map(PathBuf::from)
    }
}
