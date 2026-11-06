//! `views.callgraph_copies`: extra call-graph databases kept by a repository.
//!
//! A checkout's call-graph view costs a base database for its commit plus a
//! member database for its edits. Where the filesystem clones files the member
//! shares unchanged pages with its base, but a commit the repository has no
//! base for, or one edit to a file most of the code depends on, still costs
//! most of a full graph (1.3 to 2 GB for AFT's own repository). So the number
//! of databases is limited, including filesystem clones. A checkout over the
//! limit reads the main checkout's published graph instead, and its answers say which of its
//! files differ (`commands::callgraph_borrowed`).
//!
//! The main checkout's member and current base are exempt. Other checkout
//! copies and bases count, with each shared base counted once. At the limit,
//! the least recently used database nobody holds (no reference, live build,
//! session, pin, reader or open handle) is evicted to make room; when every
//! member is held, the new checkout borrows.
//! A checkout whose files equal the graph it would borrow borrows without
//! counting, since a copy of its own would hold the same graph. Admission and
//! registration are separate steps, so checkouts starting at the same moment
//! can overshoot the limit; later admissions evict back down.

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
    paths: &crate::root_cache::StoragePaths,
    family: &str,
    policy: crate::blob_store::v2::TrigramPolicy,
    root: &Path,
    copies: u32,
    identities: impl FnOnce() -> Option<HashMap<String, (u64, blake3::Hash)>>,
) -> Result<Admission, String> {
    let storage = paths.artifacts.as_path();
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
    // While the main checkout has no call graph, the oldest checkout that has
    // one stands in for it, so borrowers still have a graph to read.
    let Some(target) = borrow_target(&members, &main) else {
        return Ok(Admission::Own);
    };
    let target_root = target.root.clone().unwrap_or_else(|| main.clone());
    if needs_producer_rebuild(storage, family, policy, &target.scope) {
        return Ok(Admission::Own);
    }
    let borrow = Admission::Borrow {
        root: target_root.clone(),
        scope: target.scope.clone(),
    };
    if copies == 0 {
        return Ok(borrow);
    }
    let known = identities();
    if matches_target(
        storage,
        family,
        policy,
        &target.scope,
        &target_root,
        root,
        || known.clone(),
    ) {
        return Ok(borrow);
    }
    // At the limit: make room by evicting copies nobody is using. When none
    // can go, or eviction fails, this checkout borrows, so the limit holds.
    match evict_for(
        paths,
        family,
        &target.scope,
        &own,
        copies as usize,
        root,
        policy,
        known.as_ref(),
    ) {
        Ok(true) => Ok(Admission::Own),
        Ok(false) => Ok(borrow),
        Err(reason) => {
            crate::slog_warn!(
                "views callgraph copy eviction failed for={own} limit={copies}: {reason}"
            );
            Ok(borrow)
        }
    }
}

fn borrow_target<'a>(members: &'a [Member], main: &Path) -> Option<&'a Member> {
    let main_scope = super::callgraph::view_scope(main);
    members
        .iter()
        .find(|member| member.scope == main_scope)
        .or_else(|| {
            members
                .iter()
                .min_by(|a, b| (a.registered_at_ms, &a.scope).cmp(&(b.registered_at_ms, &b.scope)))
        })
}

/// Capacity can block a new base even for a checkout already registered as a
/// view. Select the same borrow target as copy saturation without readmitting
/// that checkout merely because its member row exists.
pub(crate) fn borrow_at_base_cap(
    storage: &Path,
    family: &str,
    policy: crate::blob_store::v2::TrigramPolicy,
    root: &Path,
) -> Result<Admission, String> {
    let main = super::read::main_checkout_root(root).unwrap_or_else(|| root.to_path_buf());
    let members = active_members(storage, family)?;
    let Some(target) = borrow_target(&members, &main) else {
        return Ok(Admission::Own);
    };
    if needs_producer_rebuild(storage, family, policy, &target.scope) {
        return Ok(Admission::Own);
    }
    Ok(Admission::Borrow {
        root: target.root.clone().unwrap_or(main),
        scope: target.scope.clone(),
    })
}

/// Counts and evicts idle databases under the same storage lock.
/// `Ok(false)` when every remaining copy is in use.
fn evict_for(
    paths: &crate::root_cache::StoragePaths,
    family: &str,
    target: &str,
    own: &str,
    copies: usize,
    root: &Path,
    policy: crate::blob_store::v2::TrigramPolicy,
    known: Option<&HashMap<String, (u64, blake3::Hash)>>,
) -> Result<bool, String> {
    let storage = paths.artifacts.as_path();
    // View-directory removal refuses a cache whose durable owner is unknown;
    // declare it, as storage retention does before it evicts.
    crate::root_cache::register_durable_owner(paths).map_err(|error| error.to_string())?;
    let deadline = Instant::now() + EVICTION_BUDGET;
    let lock = crate::blob_store::v3::ReservationLock::acquire_until(storage, deadline)
        .map_err(|error| error.to_string())?;
    let mut registry =
        crate::blob_store::v3::RegistryWriter::open_for_barrier(storage, family, &lock)
            .map_err(|error| error.to_string())?;
    for _ in 0..EVICTION_CANDIDATES {
        if Instant::now() >= deadline {
            return Ok(false);
        }
        let (owned, exempt_base, reusable) = database_count(
            storage,
            family,
            &mut registry,
            target,
            root,
            policy,
            known,
            deadline,
        )?;
        // A different base and the checkout's mutable copy are two databases,
        // even on filesystems where their initial pages are cloned.
        let needed = if reusable.is_some() { 1 } else { 2 };
        if owned + needed <= copies {
            registry
                .covering_measurement_until(deadline)
                .map_err(|error| error.to_string())?;
            return Ok(true);
        }
        let mut keep = vec![target, own];
        if let Some(base) = exempt_base.as_deref() {
            keep.push(base);
        }
        if let Some(base) = reusable.as_deref() {
            keep.push(base);
        }
        let evicted = crate::storage_retention::evict_least_recent_v3_member(
            storage,
            family,
            &mut registry,
            SUFFIX,
            &keep,
            EVICTION_CANDIDATES,
            deadline,
        )
        .map_err(|error| error.to_string())?;
        match evicted {
            Some(scope) => {
                crate::slog_info!("views callgraph copy evicted scope={scope} for={own}")
            }
            None => return Ok(false),
        }
    }
    Ok(false)
}

/// Count member identities, not referrer edges: several checkouts can share
/// one base. Only the target's current base is exempt; its old bases count.
fn database_count(
    storage: &Path,
    family: &str,
    registry: &mut crate::blob_store::v3::RegistryWriter<'_>,
    target: &str,
    root: &Path,
    policy: crate::blob_store::v2::TrigramPolicy,
    known: Option<&HashMap<String, (u64, blake3::Hash)>>,
    deadline: Instant,
) -> Result<(usize, Option<String>, Option<String>), String> {
    let target_dir = crate::blob_store::v3::view_dir(storage, family, target)
        .map_err(|error| error.to_string())?;
    let exempt_base = super::ViewStore::existing_dir(target_dir).and_then(|store| {
        let generation = store.current_generation_read_only().ok()??;
        let manifest = store.load_manifest_v2(&generation).ok()?;
        Some(manifest.base_header()?.scope.clone())
    });
    let members: Vec<(String, String, String)> = registry.with_barrier(|tx| {
        Ok(tx.prepare("SELECT scope,kind,view_dir FROM members WHERE state!='evicting' AND (kind='base' OR (kind='view' AND scope LIKE '%.callgraph'))")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }).map_err(|error| error.to_string())?;
    let mut count = 0;
    let mut reusable = None;
    for (scope, kind, stored) in members {
        if Instant::now() >= deadline {
            return Err("call-graph database census exceeded the admission budget".into());
        }
        if scope == target {
            continue;
        }
        if kind == "view" {
            count += 1;
            continue;
        }
        let dir = crate::blob_store::v3::view_dir(storage, family, &scope)
            .map_err(|error| error.to_string())?;
        if dir != Path::new(&stored) {
            return Err("unexpected base directory".into());
        }
        let Some(store) = super::ViewStore::existing_dir(dir) else {
            continue;
        };
        let Some(generation) = store
            .current_generation_read_only()
            .map_err(|error| error.to_string())?
        else {
            continue;
        };
        if store
            .derived_path(&generation)
            .map_err(|error| error.to_string())?
            .try_exists()
            .map_err(|error| error.to_string())?
        {
            if exempt_base.as_deref() != Some(scope.as_str()) {
                count += 1;
            }
            if reusable.is_none() {
                let manifest = store
                    .load_manifest_v2(&generation)
                    .map_err(|error| error.to_string())?;
                let producers = super::manifest_v2::Producers {
                    trigram: policy.fingerprint_hex(),
                    semantic: None,
                    callgraph: crate::callgraph_extractor::PRODUCER.into(),
                };
                let route = super::snapshot::V3Route {
                    storage: storage.to_path_buf(),
                    family: family.to_owned(),
                    scope: scope.clone(),
                };
                let paths: Vec<_> = manifest.entries().map(|(path, _)| path.clone()).collect();
                let candidate = super::tree_base::identity_for_admission(
                    super::tree_base::TreeBaseInput {
                        route: &route,
                        root,
                        paths: &paths,
                        policy,
                        producers: &producers,
                        ignore_fingerprint: manifest.header().ignore_fingerprint.clone(),
                    },
                    deadline,
                    known,
                );
                if candidate.is_some_and(|identity| identity.scope == scope) {
                    reusable = Some(scope);
                }
            }
        }
    }
    Ok((count, exempt_base, reusable))
}

/// True when the checkout's known files equal the target's published graph,
/// so borrowing it answers exactly as a copy of its own would. Resolver
/// configuration such as `package.json` is not stored in the graph, so it
/// must equal the target checkout's own copy on disk.
fn matches_target(
    storage: &Path,
    family: &str,
    policy: crate::blob_store::v2::TrigramPolicy,
    target_scope: &str,
    target_root: &Path,
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
        super::callgraph::callgraph_inputs(checkout),
        root,
        Some(target_root),
        MATCH_DISK_READS,
        Some(&crate::commands::callgraph_borrowed::views_graph_key),
    );
    differing.complete && differing.files.is_empty()
}

/// An older extractor's publication cannot serve as a borrowed copy. The
/// checkout builds its own current graph instead; other load errors still go
/// through the strict reader, which retains their diagnostics.
pub(crate) fn needs_producer_rebuild(
    storage: &Path,
    family: &str,
    policy: crate::blob_store::v2::TrigramPolicy,
    scope: &str,
) -> bool {
    let Some(generation) = published_generation(storage, family, scope) else {
        return false;
    };
    let Ok(dir) = crate::blob_store::v3::view_dir(storage, family, scope) else {
        return false;
    };
    let Some(store) = super::ViewStore::existing_dir(dir) else {
        return false;
    };
    let Ok(manifest) = store.load_manifest_v2(&generation) else {
        return false;
    };
    manifest
        .header()
        .producers
        .has_older_callgraph_than(&super::manifest_v2::Producers {
            trigram: policy.fingerprint_hex(),
            semantic: None,
            callgraph: crate::callgraph_extractor::PRODUCER.into(),
        })
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
        callgraph: crate::callgraph_extractor::PRODUCER.into(),
    };
    let generation =
        match super::base_refs::open_callgraph_seed_v3(storage, family, scope, &producers) {
            Ok(Some(generation)) => generation,
            Ok(None) => {
                return Err((
                    "the main checkout's call graph is not published yet".into(),
                    true,
                ))
            }
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
        Err(crate::callgraph_store::CallGraphStoreError::Unavailable(reason)) => {
            Err((reason, true))
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_store::v3;
    use crate::views::base_refs::tests::{
        bind, publish_test_base, published, worker_delta, FAMILY,
    };

    struct Fixture {
        _temp: tempfile::TempDir,
        _attributes: super::super::verification::AttributeSourcesGuard,
        paths: crate::root_cache::StoragePaths,
        main: PathBuf,
        worker: PathBuf,
        _main_view: super::super::callgraph::CallgraphView,
    }

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        // These limits assume checkouts share a base for unchanged tracked files.
        // System attributes shipped with Git for Windows disable that reuse proof.
        let attributes = super::super::verification::known_attribute_sources_for_test(temp.path());
        let main = temp.path().join("main");
        std::fs::create_dir(&main).unwrap();
        std::fs::write(
            main.join("a.ts"),
            "export function target() { return 1; }\n",
        )
        .unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "core.autocrlf", "false"]);
        git(
            &main,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.test",
                "add",
                "-A",
            ],
        );
        git(
            &main,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.test",
                "commit",
                "-qm",
                "fixture",
            ],
        );
        let worker = temp.path().join("worker");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                worker.to_str().unwrap(),
                "HEAD",
            ],
        );
        std::fs::write(worker.join("extra.ts"), "export function extra() {}\n").unwrap();
        let paths = crate::root_cache::StoragePaths::same(&temp.path().join("storage"));
        let main = std::fs::canonicalize(main).unwrap();
        let worker = std::fs::canonicalize(worker).unwrap();
        let main_view = super::super::callgraph::CallgraphView::new(
            main.clone(),
            paths.clone(),
            FAMILY.into(),
            policy(),
        )
        .unwrap();
        main_view.ready_reader_for_test();
        Fixture {
            _temp: temp,
            _attributes: attributes,
            paths,
            main,
            worker,
            _main_view: main_view,
        }
    }

    fn policy() -> crate::blob_store::v2::TrigramPolicy {
        crate::blob_store::v2::TrigramPolicy {
            max_file_size: 1 << 20,
        }
    }

    fn admission(f: &Fixture, copies: u32) -> Admission {
        admit(&f.paths, FAMILY, policy(), &f.worker, copies, || None).unwrap()
    }

    fn graph_base(
        f: &Fixture,
        text: &[u8],
        last_used: i64,
    ) -> (super::super::base_refs::BaseIdentity, String) {
        let storage = &f.paths.artifacts;
        let (base, generation) = publish_test_base(storage, &[("src/a.rs", text)]);
        let dir = v3::view_dir(storage, FAMILY, &base.scope).unwrap();
        let store = super::super::ViewStore::existing_dir(dir).unwrap();
        // Copy accounting only inspects the file's presence. No graph reader
        // opens these payloads; the fixture's main graph above is a real DB.
        std::fs::write(store.derived_path(&generation).unwrap(), b"graph payload").unwrap();
        super::super::base_refs::with_base_barrier(storage, FAMILY, |tx| {
            tx.execute(
                "UPDATE members SET last_bind_ms=?2 WHERE scope=?1",
                rusqlite::params![base.scope, last_used],
            )?;
            Ok(())
        })
        .unwrap();
        (base, generation)
    }

    #[test]
    fn callgraph_copies_over_limit_evicts_lru_bases_but_keeps_references_and_pins() {
        let _serial = v3::serial_for_test();
        let f = fixture();
        let (pinned, _) = graph_base(&f, b"pinned", 0);
        let _pin =
            crate::pins::LivePin::create_v3(&f.paths.artifacts, FAMILY, &pinned.scope).unwrap();
        let (referenced, generation) = graph_base(&f, b"referenced", 1);
        drop(published(
            bind(
                &f.paths.artifacts,
                "holder",
                &referenced,
                &generation,
                worker_delta(&referenced, &generation, b"referenced"),
                None,
            )
            .unwrap(),
        ));
        let (oldest, _) = graph_base(&f, b"oldest", 2);
        let (older, _) = graph_base(&f, b"older", 3);
        let (newest, _) = graph_base(&f, b"newest", 4);
        assert_eq!(admission(&f, 4), Admission::Own);
        for (base, exists) in [
            (&pinned, true),
            (&referenced, true),
            (&oldest, false),
            (&older, false),
            (&newest, true),
        ] {
            assert_eq!(
                v3::view_dir(&f.paths.artifacts, FAMILY, &base.scope)
                    .unwrap()
                    .exists(),
                exists,
                "{}",
                base.scope
            );
        }
    }

    #[test]
    fn callgraph_copies_shared_base_counts_once_and_main_base_is_exempt() {
        let _serial = v3::serial_for_test();
        let f = fixture();
        let (base, generation) = graph_base(&f, b"shared", 0);
        for holder in ["holder-a", "holder-b"] {
            drop(published(
                bind(
                    &f.paths.artifacts,
                    holder,
                    &base,
                    &generation,
                    worker_delta(&base, &generation, b"shared"),
                    None,
                )
                .unwrap(),
            ));
        }
        assert_eq!(
            admission(&f, 1),
            Admission::Borrow {
                root: f.main.clone(),
                scope: super::super::callgraph::view_scope(&f.main),
            }
        );
        assert_eq!(
            admission(&f, 2),
            Admission::Own,
            "one base shared by two holders costs one slot"
        );
    }

    #[test]
    fn callgraph_copies_new_commit_reserves_base_and_checkout_databases() {
        let _serial = v3::serial_for_test();
        let f = fixture();
        git(&f.worker, &["add", "-A"]);
        git(
            &f.worker,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.test",
                "commit",
                "-qm",
                "diverged",
            ],
        );
        assert!(matches!(admission(&f, 1), Admission::Borrow { .. }));
        assert_eq!(admission(&f, 2), Admission::Own);
    }

    #[test]
    fn callgraph_copies_reuses_a_shared_membership_base_for_identical_tracked_edits() {
        let _serial = v3::serial_for_test();
        let f = fixture();
        let edited = "export function edited() {}\n";
        std::fs::write(f.worker.join("a.ts"), edited).unwrap();
        let held = super::super::callgraph::CallgraphView::new(
            f.worker.clone(),
            f.paths.clone(),
            FAMILY.into(),
            policy(),
        )
        .unwrap();
        held.ready_reader_for_test();
        let peer = f._temp.path().join("peer");
        git(
            &f.main,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                peer.to_str().unwrap(),
                "HEAD",
            ],
        );
        let peer = std::fs::canonicalize(peer).unwrap();
        std::fs::write(peer.join("a.ts"), edited).unwrap();
        std::fs::copy(f.worker.join("extra.ts"), peer.join("extra.ts")).unwrap();
        let admit_peer = |copies| {
            admit(&f.paths, FAMILY, policy(), &peer, copies, || {
                Some(
                    ["a.ts", "extra.ts"]
                        .into_iter()
                        .map(|name| {
                            let bytes = std::fs::read(peer.join(name)).unwrap();
                            (
                                name.to_owned(),
                                (
                                    bytes.len() as u64,
                                    crate::io_accounting::source_hash(&bytes),
                                ),
                            )
                        })
                        .collect(),
                )
            })
            .unwrap()
        };
        assert!(matches!(admit_peer(2), Admission::Borrow { .. }));
        assert_eq!(
            admit_peer(3),
            Admission::Own,
            "the existing membership base must not cost another slot"
        );
    }

}
