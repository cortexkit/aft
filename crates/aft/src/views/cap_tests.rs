use std::path::{Path, PathBuf};

use crate::blob_store::v3;
use crate::context::AppContext;
use crate::protocol::RawRequest;

use super::base_builder::{register_base_in, LIVE_BASES_PER_FAMILY};
use super::base_refs::{base_scope_for, with_base_barrier};

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

struct HeldFamily {
    _temp: tempfile::TempDir,
    _attributes: super::verification::AttributeSourcesGuard,
    _main: AppContext,
    _pins: Vec<crate::pins::LivePin>,
    worker: AppContext,
    root: PathBuf,
    storage: PathBuf,
    family: String,
}

impl HeldFamily {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let attributes = super::verification::known_attribute_sources_for_test(temp.path());
        let main = temp.path().join("main");
        std::fs::create_dir(&main).unwrap();
        std::fs::write(
            main.join("lib.rs"),
            "pub fn caller() { target(); }\npub fn target() {}\n",
        )
        .unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "core.autocrlf", "false"]);
        git(&main, &["add", "."]);
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
        let root = temp.path().join("worker");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                root.to_str().unwrap(),
                "HEAD",
            ],
        );
        let root = root.canonicalize().unwrap();
        let main = main.canonicalize().unwrap();
        let storage = temp.path().join("storage");
        let config = crate::config::Config {
            project_root: Some(main.clone()),
            storage_dir: Some(storage.clone()),
            indexes: crate::config::IndexesConfig {
                trigram: true,
                semantic: false,
                callgraph: true,
            },
            views: crate::config::ViewsConfig {
                enabled: true,
                callgraph_copies: u32::MAX,
                ..Default::default()
            },
            ..Default::default()
        };
        let main_ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config.clone(),
        );
        main_ctx.set_canonical_cache_root(main.clone());
        main_ctx.callgraph_view_reader().unwrap().unwrap();
        let family = main_ctx.memoized_artifact_cache_key(&main);
        with_base_barrier(&storage, &family, |tx| {
            let count: i64 =
                tx.query_row("SELECT COUNT(*) FROM members WHERE kind='base'", [], |r| {
                    r.get(0)
                })?;
            for index in count..LIVE_BASES_PER_FAMILY {
                register_base_in(tx, &storage, &family, &base_scope_for(&[index as u8; 32]))?;
            }
            Ok(())
        })
        .unwrap();
        let scopes: Vec<String> = v3::LedgerReader::open_existing(&storage, &family)
            .unwrap()
            .read(|c| {
                Ok(c.prepare("SELECT scope FROM members WHERE kind='base'")?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?)
            })
            .unwrap();
        assert_eq!(scopes.len(), LIVE_BASES_PER_FAMILY as usize);
        let pins = scopes
            .iter()
            .map(|scope| crate::pins::LivePin::create_v3(&storage, &family, scope).unwrap())
            .collect();
        std::fs::write(
            root.join("lib.rs"),
            "pub fn caller() { target(); }\npub fn worktree_only() { target(); }\npub fn target() {}\n",
        )
        .unwrap();
        let worker = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.clone()),
                ..config
            },
        );
        worker.set_canonical_cache_root(root.clone());
        Self {
            _temp: temp,
            _attributes: attributes,
            _main: main_ctx,
            _pins: pins,
            worker,
            root,
            storage,
            family,
        }
    }

    fn cap_hits(&self) -> i64 {
        v3::LedgerReader::open_existing(&self.storage, &self.family)
            .unwrap()
            .read(|c| {
                Ok(
                    c.query_row("SELECT live_base_cap_hits FROM sweep_state", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap()
    }

    fn assert_no_failure(&self) {
        let health = self.worker.try_health_snapshot(&self.root);
        assert_eq!(self.worker.view_load_failure_total_for_test(), 0);
        assert!(health.view_load_failures.is_empty(), "{health:?}");
        assert!(health.view_load_failure_last.is_none(), "{health:?}");
        assert!(!self
            .worker
            .degraded_reasons()
            .iter()
            .any(|r| r == "view_load_failed"));
    }
}

#[test]
fn views_live_base_cap_held_callgraph_borrows_main_with_existing_note() {
    let _serial = v3::serial_for_test();
    let f = HeldFamily::new();
    let response = crate::commands::callers::handle_callers(
        &RawRequest {
            id: "cap".into(),
            command: "callers".into(),
            lsp_hints: None,
            session_id: None,
            params: serde_json::json!({"file": f.root.join("lib.rs"), "symbol": "target"}),
        },
        &f.worker,
    );
    assert!(response.success, "{:#}", response.data);
    let text = response.data.to_string();
    assert!(text.contains("borrowed from"), "{text}");
    assert!(
        response.data["borrowed_coverage"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("lib.rs")),
        "{text}"
    );
    assert_eq!(response.data["complete"], false, "{text}");
    assert!(!text.contains("This is an AFT bug"), "{text}");
    assert!(f.worker.borrowed_callgraph_view().is_some());
    assert!(f.cap_hits() > 0);
    f.assert_no_failure();
}

#[test]
fn views_live_base_cap_held_text_is_retryable_without_failure() {
    let _serial = v3::serial_for_test();
    let f = HeldFamily::new();
    let request = RawRequest {
        id: "cap".into(),
        command: "grep".into(),
        lsp_hints: None,
        session_id: None,
        params: serde_json::json!({"path": f.root, "pattern": "target"}),
    };
    let response = crate::commands::grep::handle_grep(&request, &f.worker);
    assert_eq!(response.data["code"], "busy", "{:#}", response.data);
    assert_eq!(response.data["retryable"], true, "{:#}", response.data);
    assert!(!response.data.to_string().contains("This is an AFT bug"));
    f.assert_no_failure();
    let hits = f.cap_hits();
    assert!(hits > 0);
    crate::commands::grep::handle_grep(&request, &f.worker);
    assert_eq!(f.cap_hits(), hits, "query traffic respects the retry timer");
    f.worker.view_load_retry_due_for_test();
    crate::commands::grep::handle_grep(&request, &f.worker);
    assert!(f.cap_hits() > hits, "the timer retries a capped text load");
    f.assert_no_failure();
}
