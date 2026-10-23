use super::*;

#[test]
fn requested_paths_are_relative_files_listed_once_and_at_most_64() {
    let paths = |list: &[&str]| list.iter().map(|path| path.to_string()).collect::<Vec<_>>();
    assert_eq!(
        check_requested_paths(&paths(&["out.json", "dist/app.bin"])),
        Ok(())
    );
    let too_many: Vec<String> = (0..=MAX_RETURN_PATHS).map(|n| format!("f{n}")).collect();
    assert_eq!(
        check_requested_paths(&too_many),
        Err("return_paths lists 65 paths; at most 64 are allowed".into())
    );
    let refused = [
        ("/etc/passwd", "must be relative to workdir"),
        ("../outside.txt", "must not use .."),
        ("dist/../../outside.txt", "must not use .."),
        ("dist/", "names a directory"),
        (".", "names a directory"),
        ("", "empty path"),
    ];
    for (path, reason) in refused {
        let error = check_requested_paths(&paths(&[path])).unwrap_err();
        assert!(error.contains(reason), "{path:?}: {error}");
    }
    assert_eq!(
        check_requested_paths(&paths(&["a.txt", "a.txt"])),
        Err("return_paths lists a.txt twice".into())
    );
}

#[test]
fn report_lists_each_path_once_with_its_size_or_reason() {
    let report = render_report(&[
        ReturnedPath {
            checkout_path: None,
            path: "dist/app.bin".into(),
            outcome: ReturnOutcome::Returned { bytes: 430_112 },
        },
        ReturnedPath {
            checkout_path: None,
            path: "dist/one.txt".into(),
            outcome: ReturnOutcome::Returned { bytes: 1 },
        },
        ReturnedPath {
            checkout_path: None,
            path: "dist/absent.txt".into(),
            outcome: ReturnOutcome::NotReturned {
                reason: NotReturned::Runner {
                    cause: "missing".into(),
                },
            },
        },
        ReturnedPath {
            checkout_path: None,
            path: "dist/large.bin".into(),
            outcome: ReturnOutcome::NotReturned {
                reason: NotReturned::Runner {
                    cause: "over_cap".into(),
                },
            },
        },
    ]);
    assert_eq!(
        report,
        "Files requested back from the server (return_paths):\n  \
         dist/app.bin: returned (430112 bytes)\n  \
         dist/one.txt: returned (1 byte)\n  \
         dist/absent.txt: not returned (no such file on the server)\n  \
         dist/large.bin: not returned (it would take the total past the 10 MiB limit)"
    );
}

#[test]
fn a_local_non_regular_path_is_reported_in_the_approved_words() {
    let report = render_report(&[ReturnedPath {
        checkout_path: None,
        path: "dist/app.bin".into(),
        outcome: ReturnOutcome::NotReturned {
            reason: NotReturned::NotRegular,
        },
    }]);
    assert_eq!(
        report,
        "Files requested back from the server (return_paths):\n  \
         dist/app.bin: not returned (a symlink or other non-regular file is already at this path; AFT left it unchanged)"
    );
}

#[cfg(unix)]
mod receiving {
    use super::super::*;
    use crate::bash_background::persistence::PinnedDir;
    use crate::exec_remote::return_vectors::{
        return_vector, return_vector_paths, return_vector_records, RETURN_VECTORS,
    };
    use crate::exec_remote::types::StreamRecord;
    use base64::Engine;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::Arc;

    /// A private staging folder outside the project, like a task's control
    /// folder.
    fn private_folder() -> (tempfile::TempDir, Arc<PinnedDir>) {
        let dir = tempfile::tempdir().unwrap();
        let pinned = Arc::new(PinnedDir::open(&fs::canonicalize(dir.path()).unwrap()).unwrap());
        (dir, pinned)
    }

    fn receiver(name: &str, root: &Path, private: &Arc<PinnedDir>) -> ReturnReceiver {
        let cap = return_vector(name)["cap_bytes"].as_u64().unwrap();
        ReturnReceiver::new(
            return_vector_paths(name),
            cap,
            root,
            root,
            None,
            private.clone(),
            "task-1",
        )
        .unwrap()
    }

    /// Feed a vector's returned-file records to a receiver rooted at `root`.
    fn receive(name: &str, root: &Path) -> Vec<ReturnedPath> {
        let (_dir, private) = private_folder();
        let mut receiver = receiver(name, root, &private);
        for record in return_vector_records(name) {
            match record {
                StreamRecord::ReturnChunk(chunk) => receiver.chunk(&chunk, &mut |_| Ok(())),
                StreamRecord::ReturnManifest(manifest) => {
                    return receiver.manifest(&manifest, &mut |_| Ok(()))
                }
                _ => {}
            }
        }
        panic!("{name} has no manifest")
    }

    /// Every file under `root`, relative to it, sorted.
    fn files_under(root: &Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(directory) = stack.pop() {
            for entry in fs::read_dir(directory).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let relative = path.strip_prefix(root).unwrap();
                    found.push(relative.to_string_lossy().into_owned());
                }
            }
        }
        found.sort();
        found
    }

    #[test]
    fn published_vectors_place_exactly_the_verified_files() {
        for (name, _, _) in RETURN_VECTORS {
            let dir = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(dir.path()).unwrap();
            let report = receive(name, &root);
            let vector = return_vector(name);
            let expect = &vector["expect"];
            let mut expected_files = Vec::new();
            if let Some(code) = expect["error"]["code"].as_str() {
                for entry in &report {
                    assert_eq!(
                        entry.outcome,
                        ReturnOutcome::NotReturned {
                            reason: NotReturned::Unverified { code: code.into() }
                        },
                        "{name}"
                    );
                }
            } else {
                for (entry, expected) in report.iter().zip(expect["files"].as_array().unwrap()) {
                    assert_eq!(entry.path, expected["path"].as_str().unwrap(), "{name}");
                    if let Some(cause) = expected["cause"].as_str() {
                        assert_eq!(
                            entry.outcome,
                            ReturnOutcome::NotReturned {
                                reason: NotReturned::Runner {
                                    cause: cause.into()
                                }
                            },
                            "{name}"
                        );
                        continue;
                    }
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(expected["data"].as_str().unwrap())
                        .unwrap();
                    assert_eq!(
                        entry.outcome,
                        ReturnOutcome::Returned {
                            bytes: data.len() as u64
                        },
                        "{name}"
                    );
                    assert_eq!(fs::read(root.join(&entry.path)).unwrap(), data, "{name}");
                    expected_files.push(entry.path.clone());
                }
                assert_eq!(report.len(), expect["files"].as_array().unwrap().len());
            }
            expected_files.sort();
            // Nothing else is left behind: no failed path and no staging file.
            assert_eq!(files_under(&root), expected_files, "{name}");
        }
    }

    #[test]
    fn digest_mismatch_writes_nothing_and_keeps_the_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir(root.join("dist")).unwrap();
        fs::write(root.join("dist/app.bin"), b"older build").unwrap();
        let report = receive("refuse-digest-mismatch", &root);
        assert_eq!(
            report[0].outcome,
            ReturnOutcome::NotReturned {
                reason: NotReturned::Unverified {
                    code: "digest_mismatch".into()
                }
            }
        );
        assert_eq!(fs::read(root.join("dist/app.bin")).unwrap(), b"older build");
        assert_eq!(files_under(&root), ["dist/app.bin"]);
    }

    #[test]
    fn over_cap_path_writes_nothing_beside_a_returned_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir(root.join("dist")).unwrap();
        fs::write(root.join("dist/large.bin"), b"kept").unwrap();
        let report = receive("failed-over-cap", &root);
        assert_eq!(report[0].outcome, ReturnOutcome::Returned { bytes: 5 });
        assert_eq!(
            report[1].outcome,
            ReturnOutcome::NotReturned {
                reason: NotReturned::Runner {
                    cause: "over_cap".into()
                }
            }
        );
        assert_eq!(fs::read(root.join("dist/hello.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(root.join("dist/large.bin")).unwrap(), b"kept");
    }

    #[test]
    fn read_error_after_chunks_writes_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let report = receive("read-error-after-chunks", &root);
        assert_eq!(
            report[0].outcome,
            ReturnOutcome::NotReturned {
                reason: NotReturned::Runner {
                    cause: "read_error".into()
                }
            }
        );
        assert_eq!(files_under(&root), ["dist/ok.txt"]);
    }

    #[test]
    fn symlinked_parent_leading_out_of_the_project_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("project");
        let outside = base.join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("dist")).unwrap();
        // The runner reports both files as returned and verified.
        let report = receive("two-files-one-empty", &root);
        for entry in &report {
            assert_eq!(
                entry.outcome,
                ReturnOutcome::NotReturned {
                    reason: NotReturned::OutsideProject
                },
                "{}",
                entry.path
            );
        }
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        assert_eq!(files_under(&root), Vec::<String>::new());
    }

    #[test]
    fn a_destination_symlink_is_refused_and_never_followed() {
        // A symlink at the destination is not replaced or followed, whether
        // it points outside the project or at another file inside it.
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("project");
        let outside = base.join("outside.txt");
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(&outside, b"not ours").unwrap();
        fs::write(root.join("kept.txt"), b"inside").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("dist/hello.txt")).unwrap();
        std::os::unix::fs::symlink(root.join("kept.txt"), root.join("dist/empty.txt")).unwrap();
        let report = receive("two-files-one-empty", &root);
        for entry in &report {
            assert_eq!(
                entry.outcome,
                ReturnOutcome::NotReturned {
                    reason: NotReturned::NotRegular
                },
                "{}",
                entry.path
            );
        }
        assert_eq!(fs::read(&outside).unwrap(), b"not ours");
        assert_eq!(fs::read(root.join("kept.txt")).unwrap(), b"inside");
        for link in ["dist/hello.txt", "dist/empty.txt"] {
            assert!(fs::symlink_metadata(root.join(link))
                .unwrap()
                .file_type()
                .is_symlink());
        }
    }

    #[test]
    fn a_staging_entry_swapped_for_a_symlink_never_touches_the_outside_file() {
        // Even if the staging entry were replaced by a symlink to an outside
        // file before the manifest, placement works on the handle it wrote:
        // the outside file keeps its content and mode, and nothing is placed.
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("project");
        fs::create_dir(&root).unwrap();
        let outside = base.join("id_ed25519");
        fs::write(&outside, b"secret").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
        let (_private_dir, private) = private_folder();
        let name = "three-chunks-short-last";
        let mut receiver = receiver(name, &root, &private);
        let mut manifest = None;
        for record in return_vector_records(name) {
            match record {
                StreamRecord::ReturnChunk(chunk) => receiver.chunk(&chunk, &mut |_| Ok(())),
                StreamRecord::ReturnManifest(found) => manifest = Some(found),
                _ => {}
            }
        }
        let staging = private.path().join(staging_name("task-1", 0));
        fs::remove_file(&staging).unwrap();
        std::os::unix::fs::symlink(&outside, &staging).unwrap();
        let report = receiver.manifest(&manifest.unwrap(), &mut |_| Ok(()));
        assert!(
            matches!(
                &report[0].outcome,
                ReturnOutcome::NotReturned {
                    reason: NotReturned::LocalWrite { .. }
                }
            ),
            "{report:?}"
        );
        assert_eq!(fs::read(&outside).unwrap(), b"secret");
        assert_eq!(
            fs::metadata(&outside).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert!(!root.join("dist/app.bin").exists());
        assert!(fs::symlink_metadata(root.join("dist/app.bin")).is_err());
    }

    #[test]
    fn mode_is_set_on_the_placed_file_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        // An existing file keeps its mode; a new file gets 0644.
        fs::create_dir(root.join("dist")).unwrap();
        fs::write(root.join("dist/hello.txt"), b"old").unwrap();
        fs::set_permissions(
            root.join("dist/hello.txt"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let (_private_dir, private) = private_folder();
        fs::set_permissions(private.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let name = "two-files-one-empty";
        let mut receiver = receiver(name, &root, &private);
        let mut report = None;
        for record in return_vector_records(name) {
            match record {
                StreamRecord::ReturnChunk(chunk) => receiver.chunk(&chunk, &mut |_| Ok(())),
                StreamRecord::ReturnManifest(found) => {
                    report = Some(receiver.manifest(&found, &mut |_| Ok(())))
                }
                _ => {}
            }
        }
        let report = report.unwrap();
        assert_eq!(report[0].outcome, ReturnOutcome::Returned { bytes: 5 });
        assert_eq!(report[1].outcome, ReturnOutcome::Returned { bytes: 0 });
        let mode =
            |path: &str| fs::metadata(root.join(path)).unwrap().permissions().mode() & 0o7777;
        assert_eq!(fs::read(root.join("dist/hello.txt")).unwrap(), b"hello");
        assert_eq!(mode("dist/hello.txt"), 0o600);
        assert_eq!(mode("dist/empty.txt"), 0o644);
        // The private folder is untouched apart from its staged files leaving.
        assert_eq!(
            fs::metadata(private.path()).unwrap().permissions().mode() & 0o7777,
            0o700
        );
        assert_eq!(fs::read_dir(private.path()).unwrap().count(), 0);
    }

    /// Feed a vector to `receiver`, saving what it records.
    fn feed(receiver: &mut ReturnReceiver, name: &str) -> (Vec<ReturnedPath>, StagingRecord) {
        let recorded = std::cell::RefCell::new(StagingRecord::default());
        let mut record = |staging: &StagingRecord| {
            let mut all = recorded.borrow_mut();
            all.files.extend(staging.files.iter().cloned());
            all.dirs.extend(staging.dirs.iter().cloned());
            Ok(())
        };
        let mut report = None;
        for stream_record in return_vector_records(name) {
            match stream_record {
                StreamRecord::ReturnChunk(chunk) => receiver.chunk(&chunk, &mut record),
                StreamRecord::ReturnManifest(found) => {
                    report = Some(receiver.manifest(&found, &mut record))
                }
                _ => {}
            }
        }
        (report.unwrap(), recorded.into_inner())
    }

    #[test]
    fn a_stop_before_publishing_leaves_the_destination_alone_and_replay_places_it() {
        // AFT stops after writing the temp files and before renaming them.
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir(root.join("dist")).unwrap();
        fs::write(root.join("dist/hello.txt"), b"old").unwrap();
        let (_private_dir, private) = private_folder();
        let name = "two-files-one-empty";
        let mut first = receiver(name, &root, &private);
        let (report, recorded) = with_copy_for_test(|| {
            with_before_publish_for_test(
                |_| Err(std::io::Error::other(STOPPED_FOR_TEST)),
                || feed(&mut first, name),
            )
        });
        assert!(report
            .iter()
            .all(|entry| matches!(entry.outcome, ReturnOutcome::NotReturned { .. })));
        // A crash runs no cleanup.
        std::mem::forget(first);
        assert_eq!(fs::read(root.join("dist/hello.txt")).unwrap(), b"old");
        assert!(!root.join("dist/empty.txt").exists());
        let temps = [
            root.join("dist").join(staging_name("task-1", 0)),
            root.join("dist").join(staging_name("task-1", 1)),
        ];
        for temp in &temps {
            assert!(temp.exists(), "{temp:?}");
            assert!(recorded.files.contains(temp), "{temp:?} was not recorded");
        }
        // The restart removes everything recorded, then the replay places.
        remove_recorded_staging(&recorded, "task-1");
        for temp in &temps {
            assert!(!temp.exists(), "{temp:?}");
        }
        let mut replay = receiver(name, &root, &private);
        let (report, _) = with_copy_for_test(|| feed(&mut replay, name));
        assert_eq!(report[0].outcome, ReturnOutcome::Returned { bytes: 5 });
        assert_eq!(report[1].outcome, ReturnOutcome::Returned { bytes: 0 });
        assert_eq!(fs::read(root.join("dist/hello.txt")).unwrap(), b"hello");
        assert_eq!(files_under(&root), ["dist/empty.txt", "dist/hello.txt"]);
    }

    #[test]
    fn a_temp_swapped_for_a_symlink_before_publishing_is_not_returned() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("project");
        fs::create_dir(&root).unwrap();
        let outside = base.join("id_ed25519");
        fs::write(&outside, b"secret").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
        let (_private_dir, private) = private_folder();
        let name = "three-chunks-short-last";
        let mut receiver = receiver(name, &root, &private);
        let target = outside.clone();
        let (report, _) = with_copy_for_test(|| {
            with_before_publish_for_test(
                move |temp| {
                    fs::remove_file(temp)?;
                    std::os::unix::fs::symlink(&target, temp)
                },
                || feed(&mut receiver, name),
            )
        });
        assert!(
            matches!(
                &report[0].outcome,
                ReturnOutcome::NotReturned {
                    reason: NotReturned::LocalWrite { .. }
                }
            ),
            "{report:?}"
        );
        assert_eq!(fs::read(&outside).unwrap(), b"secret");
        assert_eq!(
            fs::metadata(&outside).unwrap().permissions().mode() & 0o7777,
            0o600
        );
    }

    #[test]
    fn another_filesystem_replaces_an_existing_file_and_creates_a_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir(root.join("dist")).unwrap();
        fs::write(root.join("dist/hello.txt"), b"old").unwrap();
        fs::set_permissions(
            root.join("dist/hello.txt"),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        let report = with_copy_for_test(|| receive("two-files-one-empty", &root));
        assert_eq!(report[0].outcome, ReturnOutcome::Returned { bytes: 5 });
        assert_eq!(report[1].outcome, ReturnOutcome::Returned { bytes: 0 });
        let mode = |path: &str| {
            fs::symlink_metadata(root.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777
        };
        assert_eq!(fs::read(root.join("dist/hello.txt")).unwrap(), b"hello");
        assert_eq!(
            mode("dist/hello.txt"),
            0o640,
            "an existing file keeps its mode"
        );
        assert_eq!(mode("dist/empty.txt"), 0o644);
        assert_eq!(
            files_under(&root),
            ["dist/empty.txt", "dist/hello.txt"],
            "no temp left"
        );
    }

    /// Feed a vector, delivering the chunk at `repeat` twice, the second time
    /// with its bytes changed by `change`.
    fn feed_with_repeat(
        root: &Path,
        name: &str,
        repeat: usize,
        change: impl Fn(&mut Vec<u8>),
    ) -> Vec<ReturnedPath> {
        let (_private_dir, private) = private_folder();
        let mut receiver = receiver(name, root, &private);
        let mut chunks = 0;
        for record in return_vector_records(name) {
            match record {
                StreamRecord::ReturnChunk(chunk) => {
                    receiver.chunk(&chunk, &mut |_| Ok(()));
                    if chunks == repeat {
                        let mut again = chunk.clone();
                        change(&mut again.data.0);
                        receiver.chunk(&again, &mut |_| Ok(()));
                    }
                    chunks += 1;
                }
                StreamRecord::ReturnManifest(manifest) => {
                    return receiver.manifest(&manifest, &mut |_| Ok(()))
                }
                _ => {}
            }
        }
        panic!("{name} has no manifest")
    }

    #[test]
    fn an_exact_replay_of_the_last_chunk_is_accepted_once() {
        // A save that fails after a chunk was taken makes the stream deliver
        // that chunk again; the same offset and bytes are not new data.
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        for repeat in 0..3 {
            let report = feed_with_repeat(&root, "three-chunks-short-last", repeat, |_| {});
            assert_eq!(
                report[0].outcome,
                ReturnOutcome::Returned { bytes: 10 },
                "chunk {repeat} replayed"
            );
            assert_eq!(fs::read(root.join("dist/app.bin")).unwrap(), b"0123456789");
        }
    }

    #[test]
    fn a_replay_with_different_bytes_discards_the_job() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let report = feed_with_repeat(&root, "three-chunks-short-last", 1, |bytes| {
            bytes[0] ^= 0xff;
        });
        assert!(
            matches!(
                &report[0].outcome,
                ReturnOutcome::NotReturned {
                    reason: NotReturned::Unverified { .. }
                }
            ),
            "{report:?}"
        );
        assert!(!root.join("dist/app.bin").exists());
    }

    #[test]
    fn a_job_without_a_manifest_returns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let name = "failed-missing";
        let (_private_dir, private) = private_folder();
        let mut receiver = receiver(name, &root, &private);
        for record in return_vector_records(name) {
            if let StreamRecord::ReturnChunk(chunk) = record {
                receiver.chunk(&chunk, &mut |_| Ok(()));
            }
        }
        let report = receiver.without_manifest();
        assert!(report.iter().all(|entry| entry.outcome
            == ReturnOutcome::NotReturned {
                reason: NotReturned::NoReport
            }));
        assert_eq!(files_under(&root), Vec::<String>::new());
    }

    #[test]
    fn staging_file_is_private_and_recorded_before_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let name = "three-chunks-short-last";
        let (_private_dir, private) = private_folder();
        let mut receiver = receiver(name, &root, &private);
        let recorded = std::cell::RefCell::new(Vec::new());
        let mut record = |staging: &StagingRecord| {
            for file in &staging.files {
                assert!(
                    !file.exists(),
                    "{file:?} was created before it was recorded"
                );
            }
            for dir in &staging.dirs {
                assert!(!dir.exists(), "{dir:?} was created before it was recorded");
            }
            recorded.borrow_mut().push(staging.clone());
            Ok(())
        };
        let mut manifest = None;
        for stream_record in return_vector_records(name) {
            match stream_record {
                StreamRecord::ReturnChunk(chunk) => receiver.chunk(&chunk, &mut record),
                StreamRecord::ReturnManifest(found) => manifest = Some(found),
                _ => {}
            }
        }
        let staging = private.path().join(staging_name("task-1", 0));
        assert_eq!(
            *recorded.borrow(),
            [StagingRecord {
                files: vec![staging.clone()],
                dirs: Vec::new(),
            }],
            "one staging file, in the private folder"
        );
        assert_eq!(fs::read(&staging).unwrap(), b"0123456789");
        assert_eq!(
            files_under(&root),
            Vec::<String>::new(),
            "nothing in the project yet"
        );
        let report = receiver.manifest(&manifest.unwrap(), &mut record);
        assert_eq!(report[0].outcome, ReturnOutcome::Returned { bytes: 10 });
        assert_eq!(recorded.borrow().last().unwrap().dirs, [root.join("dist")]);
        assert_eq!(files_under(&root), ["dist/app.bin"]);
        assert!(!staging.exists());
    }

    #[test]
    fn a_failed_path_leaves_no_directory_it_created() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let report = receive("refuse-digest-mismatch", &root);
        assert!(matches!(
            report[0].outcome,
            ReturnOutcome::NotReturned { .. }
        ));
        assert!(
            !root.join("dist").exists(),
            "the staging directory is removed"
        );
    }

    #[test]
    fn recorded_staging_is_removed_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let created = root.join("out/deep");
        fs::create_dir_all(&created).unwrap();
        let staged = created.join(".aft-return-task-1-0.part");
        fs::write(&staged, b"partial").unwrap();
        let kept_dir = root.join("kept");
        fs::create_dir(&kept_dir).unwrap();
        let not_ours = kept_dir.join("notes.txt");
        fs::write(&not_ours, b"user file").unwrap();
        let record = StagingRecord {
            files: vec![staged.clone(), not_ours.clone()],
            dirs: vec![root.join("out"), created.clone(), kept_dir.clone()],
        };
        remove_recorded_staging(&record, "task-1");
        assert!(!staged.exists());
        assert!(
            !root.join("out").exists(),
            "emptied directories are removed"
        );
        assert_eq!(fs::read(&not_ours).unwrap(), b"user file");
        assert!(kept_dir.exists(), "a directory with other files stays");
    }
}
