pub use aft_base::engram_catalog::*;

#[cfg(test)]
#[allow(unused_imports)]
use serde::{Deserialize, Serialize};
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
#[allow(unused_imports)]
use std::fs;
#[cfg(test)]
#[allow(unused_imports)]
use std::io;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::Path;

#[cfg(test)]
mod tests {
    #[test]
    fn relocated_caches_are_absent_from_storage_relative_catalog() {
        let (_temp, storage) = storage_root();
        let cache = storage.with_file_name("cache");
        std::fs::create_dir(&cache).unwrap();
        let mut config = crate::config::Config {
            storage_dir: Some(storage.clone()),
            cache_dir: Some(cache.clone()),
            ..Default::default()
        };
        assert!(crate::root_cache::prepare_cache_root_for_test(&mut config).is_none());
        let artifacts = crate::root_cache::cache_root(&config);
        for domain in crate::root_cache::CACHE_DOMAINS {
            mkdir(&artifacts, domain);
        }
        mkdir(&storage, "opencode/backups");
        std::fs::write(storage.join("aft.db"), b"durable").unwrap();
        let rendered = super::render_catalog(&storage).unwrap();
        assert_engram_accepts(&storage, &rendered);
        let value: serde_json::Value = serde_json::from_slice(&rendered).unwrap();
        let paths: Vec<_> = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"aft.db"));
        assert!(paths.contains(&"opencode/backups"));
        for domain in crate::root_cache::CACHE_DOMAINS {
            assert!(!paths.contains(&domain));
        }
    }
    use super::*;
    use crate::harness::Harness;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    /// The MCP folder name on the machine whose hand-written catalog these
    /// ids continue.
    const LIVE_MCP_DIR: &str = "mcp--claude-code--b0deb2ca83f40515f2f23d13bfe131c1";

    /// A throwaway storage root named `aft`, because ENGRAM requires the
    /// catalog's `module_id` to match the folder holding it.
    fn storage_root() -> (tempfile::TempDir, PathBuf) {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join(MODULE_ID);
        fs::create_dir(&root).unwrap();
        (fixture, root)
    }

    fn mkdir(root: &Path, rel: &str) {
        fs::create_dir_all(root.join(rel)).unwrap();
    }

    fn entries(bytes: &[u8]) -> Vec<(String, String)> {
        let value: Value = serde_json::from_slice(bytes).unwrap();
        value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["entry_id"].as_str().unwrap().to_string(),
                    entry["path"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// The checks ENGRAM's `catalog::plan` and the `catalog_check` preflight
    /// apply, ported here because engram-core is not a dependency of AFT:
    /// supported schema, `module_id` equal to the holding folder's name, known
    /// classes, mechanisms and writer interactions, a string `reason` when one
    /// is given, unique entry ids, a non-blank reason and a path for excluded
    /// entries, held-read-transaction consent for portable page-db entries,
    /// root-relative `/`-separated paths with no empty, `.` or `..` part, no
    /// backslash and no drive prefix.
    ///
    /// On top of that, AFT's own promises: every declared path exists and is
    /// not a symlink (a missing one halts the whole fleet capture); every
    /// page-db entry declares `read-transaction-live`, whatever its class, as
    /// agreed with ENGRAM; a database entry names a regular file; a filetree
    /// entry names a folder or a single regular file (ENGRAM walks a file
    /// root as a one-file tree).
    fn assert_engram_accepts(root: &Path, bytes: &[u8]) {
        let value: Value = serde_json::from_slice(bytes).expect("catalog is JSON");
        assert_eq!(value["schema_version"], 1, "supported schema version");
        assert_eq!(
            value["module_id"].as_str(),
            root.file_name().and_then(|name| name.to_str()),
            "module_id must match the folder holding the catalog"
        );
        let mut seen = BTreeSet::new();
        for entry in value["entries"].as_array().expect("entries array") {
            let id = entry["entry_id"].as_str().expect("entry_id string");
            assert!(!id.is_empty(), "entry id must not be empty");
            assert!(seen.insert(id.to_string()), "duplicate entry id {id}");
            let class = entry["class"].as_str().expect("class string");
            assert!(
                ["portable", "device-local", "rebuild", "excluded"].contains(&class),
                "{id}: unknown class {class}"
            );
            let mechanism = entry["mechanism"].as_str().expect("mechanism string");
            assert!(
                [
                    "whole-db",
                    "page-db",
                    "owner-export",
                    "filetree",
                    "log-stream"
                ]
                .contains(&mechanism),
                "{id}: unknown mechanism {mechanism}"
            );
            // ENGRAM parses both fields strictly: a non-string reason or an
            // unknown writer interaction rejects the whole descriptor.
            assert!(
                entry.get("reason").is_none_or(Value::is_string),
                "{id}: reason must be a string"
            );
            if let Some(interaction) = entry.get("writer_interaction") {
                assert!(
                    [
                        "backup-api-live",
                        "quiesce-required",
                        "read-transaction-live",
                        "none"
                    ]
                    .contains(&interaction.as_str().unwrap_or("")),
                    "{id}: unknown writer_interaction {interaction}"
                );
            }
            if class == "excluded" {
                let reason = entry["reason"].as_str().unwrap_or("");
                assert!(!reason.trim().is_empty(), "{id}: excluded needs a reason");
                assert!(entry["path"].is_string(), "{id}: excluded needs a path");
            }
            if mechanism == "page-db" {
                assert_eq!(
                    entry["writer_interaction"], "read-transaction-live",
                    "{id}: page-db needs read-transaction-live"
                );
            }
            let path = entry["path"]
                .as_str()
                .expect("every AFT entry names a path");
            let bytes = path.as_bytes();
            assert!(
                !path.contains('\\')
                    && !(bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
                    && !Path::new(path).is_absolute()
                    && path
                        .split('/')
                        .all(|part| !part.is_empty() && part != "." && part != ".."),
                "{id}: path {path:?} is not a canonical root-relative path"
            );
            let meta = fs::symlink_metadata(root.join(path))
                .unwrap_or_else(|_| panic!("{id}: declared path {path} does not exist"));
            assert!(
                !meta.is_symlink(),
                "{id}: declared path {path} is a symlink"
            );
            match mechanism {
                "page-db" | "whole-db" => {
                    assert!(meta.is_file(), "{id}: database {path} is not a file")
                }
                "filetree" => assert!(
                    meta.is_dir() || meta.is_file(),
                    "{id}: filetree {path} is neither a folder nor a file"
                ),
                _ => {}
            }
        }
    }

    #[test]
    fn todays_backup_folders_keep_todays_entry_ids() {
        // The MCP folder the hand-written catalog describes is the one AFT
        // derives for the `claude-code` client.
        assert_eq!(
            Harness::Mcp {
                client: "claude-code".to_string()
            }
            .storage_segment(),
            LIVE_MCP_DIR
        );
        let (_fixture, root) = storage_root();
        for rel in [
            "opencode/backups",
            "pi/backups",
            "runner/backups",
            &format!("{LIVE_MCP_DIR}/backups"),
            "opencode/backups.parked-20260710",
            // Folders next to the backups that get entries of their own; their
            // presence must not change any `aft/backups-*` id.
            "opencode/checkpoints",
            "opencode/bash-tasks",
            "semantic",
            "index",
            "logs",
        ] {
            mkdir(&root, rel);
        }
        fs::write(root.join("aft.db"), b"").unwrap();

        let bytes = render_catalog(&root).unwrap();
        assert_engram_accepts(&root, &bytes);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        let backups = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["path"].as_str().unwrap().contains("/backups"))
            .collect::<Vec<_>>();
        let mut found = backups
            .iter()
            .map(|entry| {
                (
                    entry["entry_id"].as_str().unwrap().to_string(),
                    entry["path"].as_str().unwrap().to_string(),
                )
            })
            .collect::<Vec<_>>();
        found.sort();
        let mut expected = vec![
            ("aft/backups-opencode", "opencode/backups"),
            ("aft/backups-pi", "pi/backups"),
            ("aft/backups-runner", "runner/backups"),
            (
                "aft/backups-mcp-claude-code",
                "mcp--claude-code--b0deb2ca83f40515f2f23d13bfe131c1/backups",
            ),
            (
                "aft/backups-opencode-parked",
                "opencode/backups.parked-20260710",
            ),
        ]
        .into_iter()
        .map(|(id, path)| (id.to_string(), path.to_string()))
        .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(found, expected);

        for entry in backups {
            // Undo history is no longer backed up: these entries keep their
            // ids but are declared excluded, with a reason.
            assert_eq!(entry["class"], UNDO_BACKUPS_CLASS);
            assert_eq!(entry["class"], "excluded");
            assert_eq!(entry["mechanism"], "filetree");
            assert_eq!(
                entry["reason"],
                "earlier copies of edited files; may include secrets"
            );
            assert_eq!(entry.as_object().unwrap().len(), 5, "{entry}");
        }
    }

    #[test]
    fn a_backups_only_tree_renders_exact_bytes() {
        let (_fixture, root) = storage_root();
        mkdir(&root, "opencode/backups");
        mkdir(&root, "opencode/backups.parked-20260710");
        let expected = r#"{
  "schema_version": 1,
  "module_id": "aft",
  "entries": [
    {
      "entry_id": "aft/backups-opencode",
      "class": "excluded",
      "mechanism": "filetree",
      "path": "opencode/backups",
      "reason": "earlier copies of edited files; may include secrets"
    },
    {
      "entry_id": "aft/backups-opencode-parked",
      "class": "excluded",
      "mechanism": "filetree",
      "path": "opencode/backups.parked-20260710",
      "reason": "earlier copies of edited files; may include secrets"
    }
  ]
}
"#;
        assert_eq!(
            String::from_utf8(render_catalog(&root).unwrap()).unwrap(),
            expected
        );
    }

    #[test]
    fn only_folders_that_exist_are_declared() {
        let (_fixture, root) = storage_root();
        assert!(entries(&render_catalog(&root).unwrap()).is_empty());

        // A harness folder without backups, a `backups` that is a file, a
        // parked name with no suffix, and a folder that is not a harness.
        mkdir(&root, "pi");
        mkdir(&root, "runner");
        fs::write(root.join("runner/backups"), b"").unwrap();
        mkdir(&root, "opencode/backups.parked-");
        mkdir(&root, "not-a-harness/backups");
        let bytes = render_catalog(&root).unwrap();
        assert!(
            entries(&bytes).is_empty(),
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        assert_engram_accepts(&root, &bytes);

        mkdir(&root, "pi/backups");
        assert_eq!(
            entries(&render_catalog(&root).unwrap()),
            vec![("aft/backups-pi".to_string(), "pi/backups".to_string())]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_backup_folders_are_not_declared() {
        let (fixture, root) = storage_root();
        let elsewhere = fixture.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        mkdir(&root, "opencode");
        std::os::unix::fs::symlink(&elsewhere, root.join("opencode/backups")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("pi")).unwrap();
        assert!(entries(&render_catalog(&root).unwrap()).is_empty());
    }

    fn entry_by_id<'a>(value: &'a Value, id: &str) -> &'a Value {
        value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["entry_id"] == id)
            .unwrap_or_else(|| panic!("no entry {id} in {value}"))
    }

    #[test]
    fn a_full_storage_root_declares_every_agreed_entry() {
        let (_fixture, root) = storage_root();
        for dir in EXCLUDED_ROOT_DIRS.iter().map(|(dir, _)| *dir) {
            mkdir(&root, dir);
        }
        for rel in [
            "opencode/backups",
            "opencode/checkpoints",
            "opencode/bash-tasks",
            "pi/checkpoints",
            &format!("{LIVE_MCP_DIR}/bash-tasks"),
            "unbound/checkpoints",
            // The unbound folder never holds undo backups, so a stray one is
            // not declared.
            "unbound/backups",
            // Lock folders stay undeclared.
            ".aft",
        ] {
            mkdir(&root, rel);
        }
        for file in [
            "aft.db",
            // Sidecars are read through SQLite by the page-db capture and are
            // never declared on their own.
            "aft.db-wal",
            "aft.db-shm",
            "trusted-filter-projects.json",
            "last-update-check.json",
        ] {
            fs::write(root.join(file), b"x").unwrap();
        }

        let bytes = render_catalog(&root).unwrap();
        assert_engram_accepts(&root, &bytes);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        let mut found = value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["entry_id"].as_str().unwrap().to_string(),
                    entry["class"].as_str().unwrap().to_string(),
                    entry["mechanism"].as_str().unwrap().to_string(),
                    entry["path"].as_str().unwrap().to_string(),
                )
            })
            .collect::<Vec<_>>();
        found.sort();
        let mcp_tasks = format!("{LIVE_MCP_DIR}/bash-tasks");
        let mut expected = vec![
            ("aft/db", "device-local", "page-db", "aft.db"),
            (
                "aft/trusted-filter-projects",
                "device-local",
                "filetree",
                "trusted-filter-projects.json",
            ),
            (
                "aft/backups-opencode",
                "excluded",
                "filetree",
                "opencode/backups",
            ),
            (
                "aft/checkpoints-opencode",
                "excluded",
                "filetree",
                "opencode/checkpoints",
            ),
            (
                "aft/bash-tasks-opencode",
                "excluded",
                "filetree",
                "opencode/bash-tasks",
            ),
            (
                "aft/checkpoints-pi",
                "excluded",
                "filetree",
                "pi/checkpoints",
            ),
            (
                "aft/bash-tasks-mcp-claude-code",
                "excluded",
                "filetree",
                mcp_tasks.as_str(),
            ),
            (
                "aft/checkpoints-unbound",
                "excluded",
                "filetree",
                "unbound/checkpoints",
            ),
            (
                "aft/excluded-bash-tasks",
                "excluded",
                "filetree",
                "bash-tasks",
            ),
            (
                "aft/excluded-bash-tasks-quarantine",
                "excluded",
                "filetree",
                "bash-tasks-quarantine",
            ),
            ("aft/excluded-blobs", "excluded", "filetree", "blobs"),
            (
                "aft/excluded-callgraph",
                "excluded",
                "filetree",
                "callgraph",
            ),
            (
                "aft/excluded-checkpoints",
                "excluded",
                "filetree",
                "checkpoints",
            ),
            (
                "aft/excluded-diagnostics",
                "excluded",
                "filetree",
                "diagnostics",
            ),
            ("aft/excluded-dsym", "excluded", "filetree", "dsym"),
            (
                "aft/excluded-git-hooks",
                "excluded",
                "filetree",
                "git-hooks",
            ),
            ("aft/excluded-index", "excluded", "filetree", "index"),
            ("aft/excluded-inspect", "excluded", "filetree", "inspect"),
            ("aft/excluded-logs", "excluded", "filetree", "logs"),
            (
                "aft/excluded-onnxruntime",
                "excluded",
                "filetree",
                "onnxruntime",
            ),
            (
                "aft/excluded-rust-completed-checks",
                "excluded",
                "filetree",
                "rust-completed-checks",
            ),
            ("aft/excluded-semantic", "excluded", "filetree", "semantic"),
            ("aft/excluded-shims", "excluded", "filetree", "shims"),
            ("aft/excluded-symbols", "excluded", "filetree", "symbols"),
            ("aft/excluded-views", "excluded", "filetree", "views"),
        ]
        .into_iter()
        .map(|(id, class, mechanism, path)| {
            (
                id.to_string(),
                class.to_string(),
                mechanism.to_string(),
                path.to_string(),
            )
        })
        .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(found, expected);

        // The database entry exactly as agreed with the ENGRAM maintainers,
        // field for field.
        assert_eq!(
            *entry_by_id(&value, "aft/db"),
            serde_json::json!({
                "entry_id": "aft/db",
                "class": "device-local",
                "mechanism": "page-db",
                "path": "aft.db",
                "writer_interaction": "read-transaction-live",
            })
        );
        assert_eq!(
            *entry_by_id(&value, "aft/trusted-filter-projects"),
            serde_json::json!({
                "entry_id": "aft/trusted-filter-projects",
                "class": "device-local",
                "mechanism": "filetree",
                "path": "trusted-filter-projects.json",
            })
        );
        assert_eq!(
            entry_by_id(&value, "aft/checkpoints-opencode")["reason"],
            CHECKPOINTS_REASON
        );
        assert_eq!(
            entry_by_id(&value, "aft/bash-tasks-opencode")["reason"],
            BASH_TASKS_REASON
        );
        for (dir, reason) in EXCLUDED_ROOT_DIRS {
            assert_eq!(
                entry_by_id(&value, &format!("aft/excluded-{dir}"))["reason"],
                *reason
            );
        }
    }

    #[test]
    fn entries_need_the_right_kind_of_path_on_disk() {
        let (_fixture, root) = storage_root();
        // A file where a folder belongs and folders where files belong.
        fs::write(root.join("logs"), b"").unwrap();
        mkdir(&root, "aft.db");
        mkdir(&root, "trusted-filter-projects.json");
        mkdir(&root, "opencode");
        fs::write(root.join("opencode/checkpoints"), b"").unwrap();
        fs::write(root.join("pi"), b"").unwrap();
        let bytes = render_catalog(&root).unwrap();
        assert!(
            entries(&bytes).is_empty(),
            "{}",
            String::from_utf8_lossy(&bytes)
        );
        assert_engram_accepts(&root, &bytes);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_entries_are_not_declared() {
        let (fixture, root) = storage_root();
        let elsewhere_dir = fixture.path().join("elsewhere");
        fs::create_dir(&elsewhere_dir).unwrap();
        let elsewhere_file = fixture.path().join("elsewhere.db");
        fs::write(&elsewhere_file, b"").unwrap();
        mkdir(&root, "opencode");
        let link = |target: &Path, rel: &str| {
            std::os::unix::fs::symlink(target, root.join(rel)).unwrap();
        };
        link(&elsewhere_dir, "views");
        link(&elsewhere_dir, "opencode/checkpoints");
        link(&elsewhere_dir, "opencode/bash-tasks");
        link(&elsewhere_file, "aft.db");
        link(&elsewhere_file, "trusted-filter-projects.json");
        assert!(entries(&render_catalog(&root).unwrap()).is_empty());
    }

    #[test]
    fn mcp_clients_with_the_same_readable_name_get_distinct_excluded_ids() {
        let (_fixture, root) = storage_root();
        for client in ["a/b", "a:b"] {
            let segment = Harness::Mcp {
                client: client.to_string(),
            }
            .storage_segment();
            mkdir(&root, &format!("{segment}/checkpoints"));
            mkdir(&root, &format!("{segment}/bash-tasks"));
        }
        let bytes = render_catalog(&root).unwrap();
        assert_engram_accepts(&root, &bytes);
        let ids = entries(&bytes)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 4, "{ids:?}");
        assert_eq!(ids[0], "aft/bash-tasks-mcp-a-b");
        assert_eq!(ids[1], "aft/checkpoints-mcp-a-b");
        assert!(ids[2].starts_with("aft/bash-tasks-mcp-a-b-"), "{ids:?}");
        assert!(ids[3].starts_with("aft/checkpoints-mcp-a-b-"), "{ids:?}");
    }

    #[test]
    fn the_same_tree_renders_the_same_bytes() {
        let tree = [
            "runner/backups",
            "opencode/backups.parked-20260801",
            "opencode/backups",
            "fed--0123456789abcdef--89abcdef/backups",
            "opencode/backups.parked-20260710",
            "pi/backups",
            "pi/checkpoints",
            "opencode/bash-tasks",
            "views",
            "logs",
        ];
        let (_first_fixture, first) = storage_root();
        for rel in tree {
            mkdir(&first, rel);
        }
        let (_second_fixture, second) = storage_root();
        for rel in tree.iter().rev() {
            mkdir(&second, rel);
        }
        let bytes = render_catalog(&first).unwrap();
        assert_eq!(bytes, render_catalog(&first).unwrap());
        assert_eq!(bytes, render_catalog(&second).unwrap());
        assert_engram_accepts(&first, &bytes);
        assert!(entries(&bytes).contains(&(
            "aft/backups-fed-0123456789abcdef-89abcdef".to_string(),
            "fed--0123456789abcdef--89abcdef/backups".to_string()
        )));
    }

    #[test]
    fn a_later_parked_folder_gets_a_dated_id_and_the_first_keeps_its_id() {
        let (_fixture, root) = storage_root();
        mkdir(&root, "opencode/backups.parked-20260710");
        let before = entries(&render_catalog(&root).unwrap());
        assert_eq!(
            before,
            vec![(
                "aft/backups-opencode-parked".to_string(),
                "opencode/backups.parked-20260710".to_string()
            )]
        );

        mkdir(&root, "opencode/backups.parked-20260901");
        let bytes = render_catalog(&root).unwrap();
        assert_engram_accepts(&root, &bytes);
        assert_eq!(
            entries(&bytes),
            vec![
                (
                    "aft/backups-opencode-parked".to_string(),
                    "opencode/backups.parked-20260710".to_string()
                ),
                (
                    "aft/backups-opencode-parked-20260901".to_string(),
                    "opencode/backups.parked-20260901".to_string()
                ),
            ]
        );
    }

    #[test]
    fn mcp_clients_with_the_same_readable_name_get_distinct_ids() {
        let (_fixture, root) = storage_root();
        for client in ["a/b", "a:b"] {
            let segment = Harness::Mcp {
                client: client.to_string(),
            }
            .storage_segment();
            mkdir(&root, &format!("{segment}/backups"));
        }
        let bytes = render_catalog(&root).unwrap();
        assert_engram_accepts(&root, &bytes);
        let ids = entries(&bytes)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "aft/backups-mcp-a-b");
        assert!(ids[1].starts_with("aft/backups-mcp-a-b-"), "{ids:?}");
    }

    #[test]
    fn an_unchanged_catalog_is_not_rewritten() {
        let (_fixture, root) = storage_root();
        mkdir(&root, "opencode/backups");
        assert_eq!(write_catalog(&root).unwrap(), WriteOutcome::Written);
        let target = root.join(CATALOG_FILENAME);
        let first = fs::read(&target).unwrap();
        assert_engram_accepts(&root, &first);

        // Age the file so a rewrite would be visible even on filesystems with
        // coarse timestamps.
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        fs::File::options()
            .write(true)
            .open(&target)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(write_catalog(&root).unwrap(), WriteOutcome::Unchanged);
        assert_eq!(fs::metadata(&target).unwrap().modified().unwrap(), old);
        assert_eq!(fs::read(&target).unwrap(), first);

        mkdir(&root, "pi/backups");
        assert_eq!(write_catalog(&root).unwrap(), WriteOutcome::Written);
        assert_ne!(fs::metadata(&target).unwrap().modified().unwrap(), old);
        assert_engram_accepts(&root, &fs::read(&target).unwrap());
    }

    /// The temporary file the next atomic write on this thread creates.
    fn capture_temp_path() -> std::rc::Rc<std::cell::RefCell<Option<PathBuf>>> {
        let seen = std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot = std::rc::Rc::clone(&seen);
        crate::durability::BEFORE_TEMP_CREATE.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |path: &Path| {
                *slot.borrow_mut() = Some(path.to_path_buf());
            }));
        });
        seen
    }

    fn temp_files(root: &Path) -> Vec<String> {
        sorted_dir_names(root)
            .unwrap()
            .into_iter()
            .filter(|name| name.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn the_catalog_is_written_through_a_temp_file_that_is_then_gone() {
        let (_fixture, root) = storage_root();
        mkdir(&root, "opencode/backups");
        let temp = capture_temp_path();
        let _ = crate::durability::take();
        assert_eq!(write_catalog(&root).unwrap(), WriteOutcome::Written);

        let temp = temp.borrow().clone().expect("the write used a temp file");
        assert_eq!(
            temp.parent(),
            Some(root.as_path()),
            "temp sits beside the target"
        );
        assert!(!temp.exists(), "temp file is gone after the rename");
        assert!(temp_files(&root).is_empty());
        let target = root.join(CATALOG_FILENAME);
        assert!(crate::durability::take().iter().any(|(kind, path)| {
            *kind == crate::durability::EventKind::AtomicReplace && *path == target
        }));
        assert_eq!(fs::read(&target).unwrap(), render_catalog(&root).unwrap());
    }

    #[test]
    fn an_interrupted_write_leaves_no_partial_file_behind() {
        let (_fixture, root) = storage_root();
        mkdir(&root, "opencode/backups");
        // A non-empty directory where the catalog should go makes the final
        // rename fail after the temp file was fully written.
        let target = root.join(CATALOG_FILENAME);
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), b"x").unwrap();
        let temp = capture_temp_path();

        assert!(write_catalog(&root).is_err());
        let temp = temp
            .borrow()
            .clone()
            .expect("the write reached its temp file");
        assert!(!temp.exists(), "a failed write removes its temp file");
        assert!(temp_files(&root).is_empty());
        assert!(target.join("keep").exists(), "the target was not replaced");

        // Startup only logs the failure.
        refresh_catalog(&root);
        assert!(temp_files(&root).is_empty());
    }

    #[test]
    fn a_missing_storage_root_is_an_error_not_a_panic() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join(MODULE_ID);
        assert!(write_catalog(&root).is_err());
        refresh_catalog(&root);
        assert!(
            !root.exists(),
            "the catalog writer never creates the storage root"
        );
    }

    #[test]
    fn a_storage_root_not_named_aft_gets_no_catalog() {
        let fixture = tempfile::tempdir().unwrap();
        // An `AFT_STORAGE_DIR` such as /data/aft-store.
        let elsewhere = fixture.path().join("aft-store");
        mkdir(&elsewhere, "opencode/backups");
        assert_eq!(
            write_catalog(&elsewhere).unwrap(),
            WriteOutcome::SkippedRootName
        );
        refresh_catalog(&elsewhere);
        assert!(!elsewhere.join(CATALOG_FILENAME).exists());

        let (_fixture, root) = storage_root();
        mkdir(&root, "opencode/backups");
        assert_eq!(write_catalog(&root).unwrap(), WriteOutcome::Written);
        assert_engram_accepts(&root, &fs::read(root.join(CATALOG_FILENAME)).unwrap());
    }

    /// The MCP folders of two clients whose readable names are both `a-b`,
    /// in the sorted order the catalog assigns ids in.
    fn colliding_mcp_dirs() -> Vec<String> {
        let mut dirs = ["a/b", "a:b"]
            .into_iter()
            .map(|client| {
                Harness::Mcp {
                    client: client.to_string(),
                }
                .storage_segment()
            })
            .collect::<Vec<_>>();
        dirs.sort();
        dirs
    }

    fn id_of(bytes: &[u8], path: &str) -> String {
        entries(bytes)
            .into_iter()
            .find(|(_, entry_path)| entry_path == path)
            .map(|(id, _)| id)
            .unwrap_or_else(|| panic!("{path} not declared"))
    }

    #[test]
    fn removing_a_colliding_folder_keeps_the_survivors_id() {
        let (_fixture, root) = storage_root();
        let dirs = colliding_mcp_dirs();
        for dir in &dirs {
            mkdir(&root, &format!("{dir}/backups"));
        }
        assert_eq!(write_catalog(&root).unwrap(), WriteOutcome::Written);
        let first = fs::read(root.join(CATALOG_FILENAME)).unwrap();
        let survivor = format!("{}/backups", dirs[1]);
        let survivor_id = id_of(&first, &survivor);
        assert_eq!(
            id_of(&first, &format!("{}/backups", dirs[0])),
            "aft/backups-mcp-a-b"
        );
        assert_ne!(survivor_id, "aft/backups-mcp-a-b");

        // The folder holding the preferred id goes away; without the previous
        // catalog the survivor would now be handed that preferred id.
        fs::remove_dir_all(root.join(&dirs[0])).unwrap();
        write_catalog(&root).unwrap();
        let second = fs::read(root.join(CATALOG_FILENAME)).unwrap();
        assert_engram_accepts(&root, &second);
        assert_eq!(
            entries(&second),
            vec![(survivor_id.clone(), survivor.clone())]
        );

        // The folder coming back is a new path again and must not take the
        // survivor's id.
        mkdir(&root, &format!("{}/backups", dirs[0]));
        write_catalog(&root).unwrap();
        let third = fs::read(root.join(CATALOG_FILENAME)).unwrap();
        assert_engram_accepts(&root, &third);
        assert_eq!(id_of(&third, &survivor), survivor_id);
        assert_eq!(
            id_of(&third, &format!("{}/backups", dirs[0])),
            "aft/backups-mcp-a-b"
        );
    }

    #[test]
    fn a_fresh_id_never_repeats_a_kept_one() {
        let (_fixture, root) = storage_root();
        mkdir(&root, "opencode/backups");
        mkdir(&root, "pi/backups");
        // An older catalog gave `pi`'s usual id to the opencode folder.
        fs::write(
            root.join(CATALOG_FILENAME),
            br#"{"schema_version":1,"module_id":"aft","entries":[
                {"entry_id":"aft/backups-pi","class":"device-local","mechanism":"filetree","path":"opencode/backups"}]}"#,
        )
        .unwrap();
        let bytes = render_catalog(&root).unwrap();
        assert_engram_accepts(&root, &bytes);
        assert_eq!(id_of(&bytes, "opencode/backups"), "aft/backups-pi");
        assert_eq!(id_of(&bytes, "pi/backups"), "aft/backups-pi-2");
    }

    #[test]
    fn a_malformed_previous_catalog_starts_ids_fresh() {
        let (_fixture, root) = storage_root();
        mkdir(&root, "opencode/backups");
        let renamed = br#"{"entries":[{"entry_id":"aft/other","path":"opencode/backups"}]}"#;
        for previous in [
            &b"not json"[..],
            br#"{"entries":"nope"}"#,
            // The same path twice: neither id can be trusted.
            br#"{"entries":[{"entry_id":"aft/x","path":"opencode/backups"},
                {"entry_id":"aft/y","path":"opencode/backups"}]}"#,
        ] {
            fs::write(root.join(CATALOG_FILENAME), previous).unwrap();
            let bytes = render_catalog(&root).unwrap();
            assert_eq!(
                id_of(&bytes, "opencode/backups"),
                "aft/backups-opencode",
                "{}",
                String::from_utf8_lossy(previous)
            );
        }
        // Control: a well-formed previous catalog is honoured.
        fs::write(root.join(CATALOG_FILENAME), renamed).unwrap();
        assert_eq!(
            id_of(&render_catalog(&root).unwrap(), "opencode/backups"),
            "aft/other"
        );
    }
}
