pub use aft_base::executable_cache::image_cache::usage::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::collections::HashSet;
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
#[allow(unused_imports)]
#[cfg(windows)]
use std::path::PathBuf;

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn image_cache_mount_options_make_unseen_images_unknown() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("image"), b"not observed").unwrap();
        for (options, expected) in [
            ("", ImageUse::Absent),
            (",hidepid=0", ImageUse::Absent),
            (",hidepid=off", ImageUse::Absent),
            (",hidepid=invisible", ImageUse::Unknown),
            (",hidepid=2", ImageUse::Unknown),
            (",hidepid=1", ImageUse::Unknown),
        ] {
            let mountinfo = format!("42 1 0:4 / /proc rw - proc proc rw{options}\n");
            let snapshot = snapshot_with_processes([], Some(&mountinfo), |_, _, _| unreachable!());
            assert!(snapshot.unreadable.is_empty());
            assert_eq!(snapshot.in_use(root.path()).unwrap(), expected, "{options}");
        }
    }

    #[test]
    fn image_cache_partial_snapshot_preserves_found_with_hidden_or_denied_processes() {
        let held = tempfile::tempdir().unwrap();
        let held_path = held.path().join("image");
        fs::write(&held_path, b"open image").unwrap();
        let _open = fs::File::open(held_path).unwrap();
        let unseen = tempfile::tempdir().unwrap();
        fs::write(unseen.path().join("image"), b"not observed").unwrap();
        let child = non_dumpable_child();
        assert!(pid_alive(child.pid));
        let own_path = std::path::PathBuf::from(format!("/proc/{}", std::process::id()));
        let child_path = std::path::PathBuf::from(format!("/proc/{}", child.pid));
        let mountinfo = "42 1 0:4 / /proc rw - proc proc rw\n";
        for (paths, kind) in [
            (vec![child_path.clone()], io::ErrorKind::NotFound),
            (
                vec![child_path.join("fd"), child_path.join("maps")],
                io::ErrorKind::PermissionDenied,
            ),
        ] {
            let snapshot = snapshot_with_processes(
                [own_path.clone(), child_path.clone()],
                Some(mountinfo),
                |usage, pid, path| {
                    if pid == child.pid {
                        paths
                            .iter()
                            .map(|path| (path.clone(), io::Error::from(kind)))
                            .collect()
                    } else {
                        probe_process(usage, pid, path)
                    }
                },
            );
            assert_eq!(snapshot.unreadable.len(), 1);
            assert_eq!(snapshot.unreadable[0].pid, child.pid);
            assert_eq!(snapshot.unreadable[0].paths, paths);
            assert_eq!(snapshot.in_use(held.path()).unwrap(), ImageUse::Found);
            assert_eq!(snapshot.in_use(unseen.path()).unwrap(), ImageUse::Unknown);
        }
    }

    #[test]
    fn image_cache_process_probe_retries_exec_transition_refusal() {
        let mut attempts = 0;
        scan_process(
            || {
                attempts += 1;
                if attempts < 3 {
                    Err(io::ErrorKind::PermissionDenied.into())
                } else {
                    Ok(())
                }
            },
            || true,
        )
        .unwrap();
        assert_eq!(attempts, 3);
    }

    #[test]
    fn image_cache_process_probe_fails_closed_for_persistent_refusal() {
        let mut attempts = 0;
        let error = scan_process(
            || {
                attempts += 1;
                Err(io::ErrorKind::PermissionDenied.into())
            },
            || true,
        )
        .unwrap_err();
        assert_eq!(attempts, 3);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
