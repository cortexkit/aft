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

struct Member {
    scope: String,
    root: Option<PathBuf>,
    registered_at_ms: i64,
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
