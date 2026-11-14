pub use aft_base::path_identity::*;

#[cfg(test)]
mod tests {
    #[test]
    fn platform_temp_root_is_broad_but_tempdir_projects_are_narrow() {
        let root = super::ProjectRootId::from_path(&std::env::temp_dir())
            .unwrap()
            .into_path_buf();
        assert!(super::project_root_is_shared_temp(&root));
        let project = tempfile::tempdir().unwrap();
        let project = project.path().canonicalize().unwrap();
        assert!(!super::project_root_is_shared_temp(&project));
        assert_eq!(
            super::project_root_is_narrow(&root, &project, root.ancestors().last().unwrap()),
            Some(false)
        );
    }

    #[cfg(unix)]
    #[test]
    fn named_shared_temp_roots_use_canonical_identities() {
        for name in ["/tmp", "/var/tmp"] {
            let root = super::ProjectRootId::from_path(std::path::Path::new(name))
                .unwrap()
                .into_path_buf();
            assert!(super::project_root_is_shared_temp(&root), "{name}");
            assert!(!super::project_root_is_shared_temp(
                &root.join("my-scratch-project")
            ));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_private_temp_forms_and_platform_temp_are_broad() {
        for name in [
            std::path::PathBuf::from("/private/tmp"),
            std::path::PathBuf::from("/private/var/tmp"),
            std::env::temp_dir(),
        ] {
            let root = super::ProjectRootId::from_path(&name)
                .unwrap()
                .into_path_buf();
            assert!(
                super::project_root_is_shared_temp(&root),
                "{}",
                name.display()
            );
            assert_eq!(
                super::project_root_is_narrow(&root, &root.join("home"), std::path::Path::new("/")),
                Some(false)
            );
            assert!(!super::project_root_is_shared_temp(
                &root.join("my-scratch-project")
            ));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_temp_root_uses_the_canonical_platform_identity() {
        let temp = std::env::temp_dir();
        let root = super::ProjectRootId::from_path(&temp)
            .unwrap()
            .into_path_buf();
        assert!(super::project_root_is_shared_temp(&root));
        assert!(!super::project_root_is_shared_temp(
            &root.join("my-scratch-project")
        ));
    }

    #[test]
    fn project_root_volume_children_are_not_permission_scopes() {
        let volume = tempfile::tempdir().unwrap();
        let volume = volume.path().canonicalize().unwrap();
        let home = volume.join("users/person");
        std::fs::create_dir_all(&home).unwrap();
        for name in ["app", "workspace", "src", "code"] {
            let project = volume.join(name);
            std::fs::create_dir(&project).unwrap();
            assert_eq!(
                super::project_root_is_narrow(&project, &home, &volume),
                Some(true),
                "a project directly below a volume must stay enabled: {name}"
            );
            assert!(
                !super::directory_scope_is_narrow(&project, &home, &volume),
                "permission-card scopes must remain stricter than project roots"
            );
            #[cfg(unix)]
            assert_eq!(
                super::directory_is_protected_alias(&project, &home, &volume),
                Some(true)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn project_root_paths_allow_container_roots_but_not_protected_roots() {
        let home = std::path::Path::new("/home/person");
        let volume = std::path::Path::new("/");
        for project in ["/app", "/workspace", "/src", "/code"] {
            assert!(super::project_root_path_is_narrow(
                std::path::Path::new(project),
                home,
                volume
            ));
        }
        for protected in ["/", "/home", "/home/person"] {
            assert!(!super::project_root_path_is_narrow(
                std::path::Path::new(protected),
                home,
                volume
            ));
        }
        let mounted_volume = std::path::Path::new("/Volumes/Work");
        assert!(!super::project_root_path_is_narrow(
            mounted_volume,
            home,
            mounted_volume
        ));
        assert!(super::project_root_path_is_narrow(
            std::path::Path::new("/Volumes/Work/app"),
            home,
            mounted_volume,
        ));
    }

    #[cfg(windows)]
    #[test]
    fn project_root_paths_allow_drive_children_but_not_protected_roots() {
        let home = std::path::Path::new(r"C:\Users\person");
        let volume = std::path::Path::new(r"C:\");
        for project in [r"C:\app", r"C:\workspace", r"C:\src", r"C:\code"] {
            assert!(super::project_root_path_is_narrow(
                std::path::Path::new(project),
                home,
                volume
            ));
        }
        for protected in [volume, home, home.parent().unwrap()] {
            assert!(!super::project_root_path_is_narrow(protected, home, volume));
        }
    }

    #[test]
    fn project_root_protected_directories_remain_disabled() {
        let volume = tempfile::tempdir().unwrap();
        let volume = volume.path().canonicalize().unwrap();
        let home = volume.join("users/person");
        std::fs::create_dir_all(&home).unwrap();
        for root in [
            &volume,
            &home,
            home.parent().unwrap(),
            volume.ancestors().last().unwrap(),
        ] {
            assert_eq!(
                super::project_root_is_narrow(root, &home, &volume),
                Some(false)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn project_root_unknown_identity_is_not_admitted() {
        let volume = tempfile::tempdir().unwrap();
        let volume = volume.path().canonicalize().unwrap();
        let home = volume.join("users/person");
        std::fs::create_dir_all(&home).unwrap();
        let missing = volume.join("missing/project");
        assert_eq!(
            super::project_root_is_narrow(&missing, &home, &volume),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_root_protected_aliases_remain_disabled() {
        let volume = tempfile::tempdir().unwrap();
        let volume = volume.path().canonicalize().unwrap();
        let home = volume.join("users/person");
        std::fs::create_dir_all(&home).unwrap();
        // Following symlinks gives the same inode identities as protected
        // directories without requiring privileged bind mounts in the test.
        for (name, target) in [
            ("app", home.as_path()),
            ("workspace", home.parent().unwrap()),
            ("code", volume.as_path()),
        ] {
            let alias = volume.join(name);
            std::os::unix::fs::symlink(target, &alias).unwrap();
            assert!(super::project_root_path_is_narrow(&alias, &home, &volume));
            assert_eq!(
                super::project_root_is_narrow(&alias, &home, &volume),
                Some(false)
            );
        }
    }

    use super::*;
    use std::fs;

    /// Mount-table fixtures in the `/proc/self/mountinfo` format, judged the
    /// way a container sees its bind-mounted workspace: the project root is on
    /// a different device from its parent, and the parent is on the
    /// container's overlay root `/`.
    #[cfg(target_os = "linux")]
    mod mount_table {
        use super::super::{
            mount_root_in, project_root_is_shared_temp, project_root_path_is_narrow,
            project_root_volume_with, MountRoot,
        };
        use std::path::{Path, PathBuf};

        const OVERLAY_ROOT: &str = "1748 1 0:173 / / rw,relatime master:1 - overlay overlay rw,lowerdir=/l,upperdir=/u,workdir=/w\n";

        fn narrow_with(root: &str, read: impl FnOnce() -> std::io::Result<Vec<u8>>) -> bool {
            let root = Path::new(root);
            let home = Path::new("/root");
            let volume =
                project_root_volume_with(root, root.to_path_buf(), Some(home), read, |_| {
                    Some(PathBuf::from("/"))
                });
            volume.is_some_and(|volume| {
                project_root_path_is_narrow(root, home, &volume)
                    && !project_root_is_shared_temp(root)
            })
        }

        #[test]
        fn shared_temp_roots_are_broad_on_bind_and_tmpfs_mounts() {
            for root in ["/tmp", "/var/tmp"] {
                assert!(!narrow(
                    root,
                    &format!("1801 1748 8:1 /tmp {root} rw - ext4 /dev/sda1 rw")
                ));
                assert!(!narrow(
                    root,
                    &format!("1801 1748 0:45 / {root} rw - tmpfs tmpfs rw")
                ));
                assert!(narrow(&format!("{root}/my-scratch-project"), &format!("1801 1748 8:1 /tmp/project {root}/my-scratch-project rw - ext4 /dev/sda1 rw")));
            }
        }

        fn narrow(root: &str, mount: &str) -> bool {
            narrow_with(root, || Ok(format!("{OVERLAY_ROOT}{mount}\n").into_bytes()))
        }

        #[test]
        fn bind_of_a_subdirectory_is_narrow() {
            // `docker run -v /tmp/repro/proj-x:/proj` and `-v /tmp/repro:/data`.
            assert!(narrow(
                "/proj",
                "1801 1748 8:1 /tmp/repro/proj-x /proj ro,relatime - ext4 /dev/sda1 rw"
            ));
            assert!(narrow(
                "/data",
                "1801 1748 8:1 /tmp/repro /data ro,relatime shared:5 master:1 - ext4 /dev/sda1 rw"
            ));
            assert!(narrow(
                "/workspace",
                "1801 1748 8:1 /var/lib/docker/volumes/x/_data /workspace rw - ext4 /dev/sda1 rw"
            ));
            assert!(narrow(
                "/work space",
                "1801 1748 8:1 /srv/my\\040proj /work\\040space rw - xfs /dev/sda1 rw"
            ));
        }

        #[test]
        fn whole_filesystem_mount_is_broad() {
            let mount = "1801 1748 8:17 / /data rw,relatime shared:5 - ext4 /dev/sdb1 rw";
            assert!(!narrow("/data", mount));
            // Classified as a whole volume, not as a bind that only the
            // home-folder rule keeps broad.
            assert_eq!(
                mount_root_in(mount.as_bytes(), Path::new("/data")),
                MountRoot::WholeVolume
            );
        }

        #[test]
        fn mount_on_top_of_a_bind_decides() {
            assert!(!narrow(
                "/data",
                "1801 1748 8:1 /tmp/repro /data rw - ext4 /dev/sda1 rw\n\
                 1802 1801 8:17 / /data rw - ext4 /dev/sdb1 rw"
            ));
        }

        #[test]
        fn btrfs_subvolume_root_is_broad() {
            assert!(!narrow(
                "/data",
                "1801 1748 0:45 /@data /data rw,relatime shared:5 - btrfs /dev/sdb2 rw,space_cache=v2,subvolid=257,subvol=/@data"
            ));
            // Without a `subvol=` option a subvolume cannot be told from a bind.
            assert!(!narrow(
                "/data",
                "1801 1748 0:45 /@data /data rw - btrfs /dev/sdb2 rw,space_cache=v2"
            ));
            // A root outside the named subvolume is not understood.
            assert!(!narrow(
                "/data",
                "1801 1748 0:45 /@other /data rw - btrfs /dev/sdb2 rw,subvolid=257,subvol=/@data"
            ));
            // A bind of a folder inside the subvolume is a project.
            assert!(narrow(
                "/data",
                "1801 1748 0:45 /@data/proj /data rw - btrfs /dev/sdb2 rw,subvolid=257,subvol=/@data"
            ));
        }

        #[test]
        fn bind_of_a_home_folder_is_broad() {
            for source in ["/home/u", "/home", "/Users/u", "/root", "/var/home/u"] {
                assert!(
                    !narrow(
                        "/workspace",
                        &format!("1801 1748 8:1 {source} /workspace rw - ext4 /dev/sda1 rw")
                    ),
                    "{source}"
                );
            }
            // The account home and its ancestors count wherever they live.
            let home = Path::new("/srv/people/u");
            for source in ["/srv/people/u", "/srv/people", "/srv"] {
                let root = Path::new("/workspace");
                let volume = project_root_volume_with(
                    root,
                    root.to_path_buf(),
                    Some(home),
                    || {
                        Ok(format!(
                            "{OVERLAY_ROOT}1801 1748 8:1 {source} /workspace rw - ext4 /dev/sda1 rw\n"
                        )
                        .into_bytes())
                    },
                    |_| Some(PathBuf::from("/")),
                );
                assert_eq!(volume.as_deref(), Some(root), "{source}");
            }
        }

        #[test]
        fn filesystem_root_is_broad() {
            // A sandbox whose `/` is itself a bind of a host subdirectory.
            assert!(!narrow_with("/", || Ok(
                b"1748 317 9:5 /runner/sandboxes/x/root / ro - xfs /dev/md5 rw\n".to_vec()
            )));
        }

        #[test]
        fn non_utf8_mount_names_do_not_hide_a_bind() {
            // Linux names may hold any byte except `/` and NUL, and mountinfo
            // leaves non-UTF-8 bytes unescaped, so a mount with such bytes
            // elsewhere in the table must not stop the `/workspace` entry from
            // being parsed.
            let table = [
                OVERLAY_ROOT.as_bytes(),
                b"1802 1748 8:1 /srv/caf\xe9 /mnt/caf\xe9 rw - ext4 /dev/sda1 rw\n",
                b"1801 1748 8:1 /tmp/repro/proj-x /workspace rw - ext4 /dev/sda1 rw\n",
            ]
            .concat();
            assert!(narrow_with("/workspace", || Ok(table)));
        }

        #[test]
        fn non_utf8_mount_point_matches_its_entry() {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            let root = Path::new(OsStr::from_bytes(b"/caf\xe9"));
            let table = [
                OVERLAY_ROOT.as_bytes(),
                b"1801 1748 8:1 /srv/proj\xff /caf\xe9 rw - ext4 /dev/sda1 rw\n",
            ]
            .concat();
            assert_eq!(
                mount_root_in(&table, root),
                MountRoot::Subdirectory(PathBuf::from(OsStr::from_bytes(b"/srv/proj\xff")))
            );
            let volume = project_root_volume_with(
                root,
                root.to_path_buf(),
                Some(Path::new("/root")),
                || Ok(table.clone()),
                |_| Some(PathBuf::from("/")),
            );
            assert_eq!(volume, Some(PathBuf::from("/")));
        }

        #[test]
        fn unreadable_mount_table_keeps_the_device_answer() {
            assert!(!narrow_with("/proj", || Err(std::io::Error::other(
                "permission denied"
            ))));
            // A mount point missing from the table is not understood either.
            assert!(!narrow(
                "/proj",
                "1801 1748 8:1 /tmp/repro/proj-x /elsewhere rw - ext4 /dev/sda1 rw"
            ));
        }

        #[test]
        fn root_below_its_mount_point_keeps_its_volume() {
            let volume = project_root_volume_with(
                Path::new("/data/proj"),
                PathBuf::from("/data"),
                None,
                || panic!("the mount table is only read for a mount point root"),
                |_| panic!("no re-anchoring below a mount point"),
            );
            assert_eq!(volume, Some(PathBuf::from("/data")));
        }
    }

    #[test]
    fn lexical_normalization_preserves_parents_and_root_boundaries() {
        for (input, expected) in [
            ("../../x", "../../x"),
            ("../a/../../x", "../../x"),
            ("a/../b", "b"),
            ("a/../../x", "../x"),
        ] {
            assert_eq!(lexical_normalize(Path::new(input)), Path::new(expected));
        }
        #[cfg(unix)]
        assert_eq!(lexical_normalize(Path::new("/../../x")), Path::new("/x"));
    }

    #[cfg(windows)]
    #[test]
    fn lexical_normalization_preserves_drive_relative_parents() {
        assert_eq!(
            lexical_normalize(Path::new(r"C:..\..\x")),
            Path::new(r"C:..\..\x")
        );
        assert_eq!(
            lexical_normalize(Path::new(r"C:\..\..\x")),
            Path::new(r"C:\x")
        );
    }

    /// A fresh root directory, removed when the returned guard drops.
    fn temp_root(label: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("aft-path-identity-{label}-"))
            .tempdir()
            .expect("create temp root");
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[test]
    fn scope_key_is_stable_and_16_hex() {
        let (_root_dir, root) = temp_root("stable");
        let a = project_scope_key(&root);
        let b = project_scope_key(&root);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn scope_key_distinguishes_distinct_roots() {
        let (_a_dir, a) = temp_root("root-a");
        let (_b_dir, b) = temp_root("root-b");
        assert_ne!(project_scope_key(&a), project_scope_key(&b));
    }

    #[test]
    fn scope_key_canonicalizes_equivalent_spellings() {
        let (_root_dir, root) = temp_root("spelling");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).expect("create nested");
        // `root/nested/..` and `root/.` both canonicalize to `root`.
        assert_eq!(
            project_scope_key(&root),
            project_scope_key(&nested.join("..")),
        );
        assert_eq!(project_scope_key(&root), project_scope_key(&root.join(".")));
    }

    #[test]
    fn scope_key_total_on_non_existent_root() {
        // Non-existent path: lexical fallback, no panic, stable.
        let missing = std::env::temp_dir().join("aft-path-identity-definitely-missing-xyz/sub/..");
        let key = project_scope_key(&missing);
        assert_eq!(key.len(), 16);
    }

    /// The headline P0 invariant: a linked git worktree SHARES the main
    /// checkout's artifact cache key (same root commit → shared index, opened
    /// read-only) but gets a DISTINCT scope key (its own checkout path → its own
    /// bash/compression/backup/checkpoint namespace). Before the split, both
    /// were the same root-commit key, so worktree A's tasks/undo bled into B.
    #[test]
    fn worktree_shares_artifact_key_but_has_distinct_scope_key() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        use std::process::Command;

        let mut git_version = Command::new("git");
        let git_ok = crate::test_env::apply_hermetic_git_env(&mut git_version)
            .arg("--version")
            .output()
            .is_ok();
        if !git_ok {
            eprintln!("skipping: git not available");
            return;
        }

        let tmp = std::env::temp_dir().join(format!(
            "aft-worktree-iso-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let main = tmp.join("main");
        fs::create_dir_all(&main).expect("create main checkout");

        let run = |args: &[&str], cwd: &std::path::Path| {
            let mut command = Command::new("git");
            assert!(
                crate::test_env::apply_hermetic_git_env(command.current_dir(cwd))
                    .args(args)
                    .status()
                    .expect("run git")
                    .success(),
                "git {args:?} failed"
            );
        };
        run(&["init"], &main);
        fs::write(main.join("f.txt"), "x\n").expect("write file");
        run(&["add", "."], &main);
        run(
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e.x",
                "commit",
                "-m",
                "init",
            ],
            &main,
        );

        let worktree = tmp.join("wt");
        run(
            &[
                "worktree",
                "add",
                worktree.to_str().unwrap(),
                "-b",
                "feature",
            ],
            &main,
        );

        let main_artifact = crate::root_identity::key::artifact_cache_key(&main);
        let wt_artifact = crate::root_identity::key::artifact_cache_key(&worktree);
        let main_scope = project_scope_key(&main);
        let wt_scope = project_scope_key(&worktree);

        // Same repo (same root commit) → SHARED artifact key (worktree reuses
        // main's on-disk index, read-only).
        assert_eq!(
            main_artifact, wt_artifact,
            "worktree must share the main checkout's artifact cache key"
        );
        // Distinct checkout path → DISTINCT scope key (no bash/undo/stats bleed).
        assert_ne!(
            main_scope, wt_scope,
            "worktree must get its own per-checkout scope key"
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}

#[cfg(test)]
mod comparison_tests {
    /// Unmatched leading `..` components must survive comparison normalization:
    /// dropping them makes a path that escapes upward look like a nearby child.
    #[test]
    fn comparison_adapter_preserves_relative_parent_components() {
        for (input, expected) in [("../../x", "../../x"), ("a/../../x", "../x")] {
            assert_eq!(
                super::normalize_path(std::path::Path::new(input)),
                std::path::PathBuf::from(expected),
                "{input}"
            );
        }
    }
}
