//! The read tool judges the file it opened, not only the name it was given:
//! a hard link to a protected file and a temp name replaced after the temp
//! allowance checked it are both refused before any byte is returned.

use crate::config::Config;
use crate::context::AppContext;
use crate::language::StubProvider;
use crate::protocol::RawRequest;
use serde_json::json;
use std::path::{Path, PathBuf};

const SECRET: &str = "TOP-SECRET-CONTENT";

fn request(file: &Path) -> RawRequest {
    RawRequest {
        id: "opened-floor".into(),
        command: "read".into(),
        session_id: Some("opened-floor".into()),
        lsp_hints: None,
        params: json!({ "file": file }),
    }
}

/// A project root, a read-denied folder holding a secret, and spare folders
/// for temp and outside files. Under the crate directory rather than the
/// system temp directory, so only the folder a test names as temp is temp.
struct Fixture {
    _dir: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    secret: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir_in(crate::test_support::manifest_dir()).unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("root");
        let denied = base.join("denied");
        for path in [&root, &denied, &base.join("tmp"), &base.join("outside")] {
            std::fs::create_dir(path).unwrap();
        }
        let secret = denied.join("secret.txt");
        std::fs::write(&secret, SECRET).unwrap();
        Self {
            _dir: dir,
            base,
            root,
            secret,
        }
    }

    fn context(&self) -> AppContext {
        let mut config = Config {
            project_root: Some(self.root.clone()),
            storage_dir: Some(self.base.join("store")),
            restrict_to_project_root: false,
            ..Config::default()
        };
        config.backup.enabled = Some(false);
        config.sandbox.read_deny = vec![self.secret.parent().unwrap().to_path_buf()];
        AppContext::new(Box::new(StubProvider), config)
    }
}

#[test]
fn read_refuses_a_hard_link_to_a_read_denied_file() {
    let f = Fixture::new();
    let ctx = f.context();
    let alias = f.root.join("alias.txt");
    std::fs::hard_link(&f.secret, &alias).unwrap();
    let response = super::read::handle_read(&request(&alias), &ctx);
    assert_eq!(response.data["code"], "denied_floor", "{response:?}");
    assert_eq!(
        response.data["message"],
        format!(
            "path '{}' is denied by the access policy floor",
            alias.display()
        )
    );
    assert!(!response.data.to_string().contains(SECRET));

    // A hard link between two ordinary project names still reads.
    let plain = f.root.join("plain.txt");
    std::fs::write(&plain, "plain").unwrap();
    std::fs::hard_link(&plain, f.root.join("plain-link.txt")).unwrap();
    let response = super::read::handle_read(&request(&plain), &ctx);
    assert!(response.success, "{response:?}");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod general_temp {
    use super::*;
    use crate::access_policy::{ask_route_guard, plugin, temp, AskRoute};

    fn standalone_read(f: &Fixture, path: &Path) -> crate::protocol::Response {
        let owner = std::sync::Arc::new(());
        let _temp = temp::override_roots_for_test(vec![f.base.join("tmp")]);
        let _route = ask_route_guard(Some(AskRoute::Plugin {
            scope: plugin::Scope::new(&owner, "opened-floor"),
            once: Vec::new(),
        }));
        super::super::read::handle_read(&request(path), &f.context())
    }

    #[test]
    fn stable_temp_reads_keep_working() {
        let f = Fixture::new();
        let path = f.base.join("tmp/stable.txt");
        std::fs::write(&path, "stable temp content").unwrap();
        let response = standalone_read(&f, &path);
        assert!(response.success, "{response:?}");
        assert!(response.data.to_string().contains("stable temp content"));
    }

    #[test]
    fn temp_read_refuses_a_parent_replaced_after_the_check() {
        let f = Fixture::new();
        let folder = f.base.join("tmp/folder");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("data.txt"), "checked").unwrap();
        let outside = f.base.join("outside");
        std::fs::write(outside.join("data.txt"), SECRET).unwrap();
        let swap_folder = folder.clone();
        super::super::read::after_authorization_for_test(move || {
            std::fs::remove_dir_all(&swap_folder).unwrap();
            std::os::unix::fs::symlink(&outside, &swap_folder).unwrap();
        });
        let response = standalone_read(&f, &folder.join("data.txt"));
        assert!(!response.success, "{response:?}");
        assert!(!response.data.to_string().contains(SECRET), "{response:?}");
    }
}
