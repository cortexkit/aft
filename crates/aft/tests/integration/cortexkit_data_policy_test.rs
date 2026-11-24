use super::helpers::{user_config, AftProcess};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

struct Fixture {
    aft: AftProcess,
    _root: tempfile::TempDir,
    home: PathBuf,
    xdg_data: PathBuf,
}

impl Fixture {
    fn new(backups: bool) -> Self {
        Self::start(backups, tempfile::tempdir().unwrap(), "project")
    }

    fn start(backups: bool, root: tempfile::TempDir, project: &str) -> Self {
        let home = root.path().join("home");
        let xdg_data = root.path().join("xdg-data");
        let project = root.path().join(project);
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir(&xdg_data).unwrap();
        let aft = AftProcess::spawn_with_env(&[
            ("HOME", home.as_os_str()),
            ("XDG_DATA_HOME", xdg_data.as_os_str()),
            (
                "XDG_CONFIG_HOME",
                root.path().join("xdg-config").as_os_str(),
            ),
            ("XDG_STATE_HOME", root.path().join("xdg-state").as_os_str()),
        ]);
        let mut fixture = Self {
            _root: root,
            home,
            xdg_data,
            aft,
        };
        let response = fixture.send(json!({
            "command":"configure", "harness":"opencode", "project_root":project,
            "config": user_config(json!({"backup":{"enabled":backups}, "indexes":{"trigram":false}})),
        }));
        assert_eq!(response["success"], true, "{response}");
        fixture
    }

    fn roots(&self) -> [PathBuf; 2] {
        [
            self.home.join(".local/share/cortexkit"),
            self.xdg_data.join("cortexkit"),
        ]
    }

    fn send(&mut self, mut request: Value) -> Value {
        request["id"] = json!("cortexkit-data");
        self.aft.send(&request.to_string())
    }
}

fn put(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn init_git(path: &Path) {
    let result = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .arg(path)
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
}

#[test]
fn cortexkit_data_file_tools_apply_normal_access_and_keep_exceptions() {
    let mut f = Fixture::new(true);
    for root in f.roots() {
        let ordinary = root.join("module/notes.txt");
        put(&ordinary, "original bytes\n");
        let read = f.send(json!({"command":"read", "file":ordinary}));
        assert_eq!(read["success"], true, "{read}");
        assert!(read.to_string().contains("original bytes"), "{read}");
        let write = f.send(json!({"command":"write", "file":ordinary,"content":"updated bytes\n"}));
        assert_eq!(write["success"], true, "{write}");
        assert_eq!(fs::read_to_string(&ordinary).unwrap(), "updated bytes\n");

        let public = root.join("ckbus/public.txt");
        put(&public, "public bytes\n");
        let read = f.send(json!({"command":"read", "file":public}));
        assert_eq!(read["success"], true, "{read}");
        assert!(read.to_string().contains("public bytes"), "{read}");
        let write = f.send(json!({"command":"write", "file":public,"content":"edited\n"}));
        assert_eq!(write["code"], "denied_floor", "{write}");
        assert!(
            write
                .to_string()
                .contains("the module rewrites these files while it runs"),
            "{write}"
        );
        assert_eq!(fs::read_to_string(&public).unwrap(), "public bytes\n");
    }
    let state = f.home.join(".local/state/cortexkit/module/probe.txt");
    put(&state, "private state\n");
    let reply = f.send(json!({"command":"read", "file":state}));
    assert_eq!(reply["code"], "denied_floor", "{reply}");
    assert!(
        reply.to_string().contains("module's state folder"),
        "{reply}"
    );
}

#[test]
fn cortexkit_data_recursive_delete_refuses_first_protected_entry_before_mutating() {
    // System-temp trees have an independent SQLite guard, so place the project
    // outside that tier to exercise the CortexKit data floor in both delete walks.
    let parent = crate::test_helpers::manifest_dir().join("../../target/scratch/cortexkit-data");
    fs::create_dir_all(&parent).unwrap();
    for backups in [true, false] {
        let root = tempfile::tempdir_in(&parent).unwrap();
        let mut f = Fixture::start(backups, root, "home/.local/share/cortexkit/module");
        let tree = f.roots()[0].join("module/tree");
        let ordinary = tree.join("notes.txt");
        let protected = tree.join("nested/caps.db");
        put(&ordinary, "keep me\n");
        put(&protected, "keep the database\n");
        let reply = f.send(json!({"command":"delete_file", "file":tree,"recursive":true}));
        assert_eq!(reply["code"], "denied_floor", "{reply}");
        assert!(reply.to_string().contains("caps.db"), "{reply}");
        assert_eq!(fs::read_to_string(ordinary).unwrap(), "keep me\n");
        assert_eq!(
            fs::read_to_string(protected).unwrap(),
            "keep the database\n"
        );
    }
}

#[test]
fn cortexkit_data_file_move_checks_both_locations() {
    let mut f = Fixture::new(true);
    let root = &f.roots()[0];
    let ordinary = root.join("module/notes.txt");
    put(&ordinary, "ordinary bytes\n");
    for relative in [
        "module/caps.db",
        "module/api-token.json",
        "signing/actor.json",
        "ckbus/public.txt",
    ] {
        let protected = root.join(relative);
        fs::create_dir_all(protected.parent().unwrap()).unwrap();
        let reply = f.send(json!({"command":"move_file", "file":ordinary,"destination":protected}));
        assert_eq!(reply["code"], "denied_floor", "{reply}");
        assert!(ordinary.exists());
        assert!(!protected.exists());

        put(&protected, "protected bytes\n");
        let dest = root.join("module/other.txt");
        let reply = f.send(json!({"command":"move_file", "file":protected,"destination":dest}));
        assert_eq!(reply["code"], "denied_floor", "{reply}");
        assert_eq!(fs::read_to_string(protected).unwrap(), "protected bytes\n");
        assert!(!dest.exists());
    }
}

#[test]
fn cortexkit_data_walks_skip_protected_entries() {
    let mut f = Fixture::new(true);
    let module = f.roots()[1].join("ckbus");
    put(&module.join("open.rs"), "fn aft_probe_public() {}\n");
    put(&module.join("caps.db"), "fn aft_probe_database() {}\n");
    put(&module.join("token.rs"), "fn aft_probe_credential() {}\n");
    init_git(&module);
    for (command, args, rows) in [
        (
            "grep",
            json!({"path":module,"pattern":"aft_probe"}),
            "matches",
        ),
        ("glob", json!({"path":module,"pattern":"**/*"}), "files"),
        (
            "semantic_search",
            json!({"path":module,"pattern":"aft_probe"}),
            "results",
        ),
    ] {
        let mut args = args;
        args["command"] = json!(command);
        let reply = f.send(args);
        assert_eq!(reply["success"], true, "{command}: {reply}");
        let hits = reply[rows].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{command}: {reply}");
        assert!(
            hits[0].to_string().contains("open.rs"),
            "{command}: {reply}"
        );
        let skipped = reply["skipped_files"].as_array().unwrap();
        for name in ["caps.db", "token.rs"] {
            assert!(
                skipped.iter().any(|skip| skip["reason"] == "denied_floor"
                    && skip["file"].as_str().unwrap().ends_with(name)),
                "{command}: {reply}"
            );
        }
    }
    let reply = f.send(json!({"command":"outline", "directory":module}));
    assert_eq!(reply["success"], true, "{reply}");
    assert!(
        reply["text"].as_str().unwrap().contains("aft_probe_public"),
        "{reply}"
    );
    assert!(
        !reply.to_string().contains("aft_probe_credential"),
        "{reply}"
    );
    assert!(
        reply["skipped_files"].to_string().contains("token.rs"),
        "{reply}"
    );
}

#[test]
fn cortexkit_data_name_rules_do_not_restrict_indexing_an_ordinary_repository() {
    let mut f = Fixture::new(true);
    let repo = f._root.path().join("other-project");
    for name in ["token.rs", "secret_rules.rs", "credentials.md"] {
        put(&repo.join(name), "aft_probe_ordinary\n");
    }
    init_git(&repo);
    let reply =
        f.send(json!({"command":"semantic_search", "path":repo,"pattern":"aft_probe_ordinary"}));
    assert_eq!(reply["success"], true, "{reply}");
    let results = reply["results"].as_array().unwrap();
    assert_eq!(results.len(), 3, "{reply}");
    for name in ["token.rs", "secret_rules.rs", "credentials.md"] {
        assert!(
            results
                .iter()
                .any(|hit| hit["file"].as_str().unwrap().ends_with(name)),
            "{reply}"
        );
    }
    assert!(reply.get("skipped_files").is_none(), "{reply}");
}
