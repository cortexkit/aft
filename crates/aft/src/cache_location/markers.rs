pub use aft_base::cache_location::markers::*;

#[cfg(test)]
#[allow(unused_imports)]
use super::{canonical_root, pin_cache_path};
#[cfg(test)]
#[allow(unused_imports)]
use crate::fs_lock;
#[cfg(test)]
#[allow(unused_imports)]
use crate::process_identity::process_start_time_ms;
#[cfg(test)]
#[allow(unused_imports)]
use serde::{Deserialize, Serialize};
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::HashMap;
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(test)]
use std::fs;
#[cfg(test)]
#[allow(unused_imports)]
use std::fs::File;
#[cfg(test)]
#[allow(unused_imports)]
use std::io;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::{Mutex, OnceLock};
#[cfg(test)]
#[allow(unused_imports)]
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_identity::{
        clear_process_start_time_for_test, set_process_start_time_for_test,
    };
    #[test]
    fn durability_read_marker_count() {
        let dir = tempfile::tempdir().unwrap();
        crate::durability::take();
        let marker = ReadMarker::create(dir.path(), "current").unwrap();
        marker.touch().unwrap();
        assert!(protected_read_marker_exists(dir.path(), "current"));
        drop(marker);
        let events = crate::durability::take();
        assert_eq!(crate::durability::sync_count(&events), 0, "{events:?}");
    }

    #[test]
    fn read_marker_file_is_private_and_touchable() {
        let dir = tempfile::tempdir().unwrap();
        let marker = ReadMarker::create(dir.path(), "current").unwrap();
        assert!(marker.path().is_file());
        marker.touch().unwrap();
        let bytes = fs::read(marker.path()).unwrap();
        let parsed: ReadMarkerMetadata = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.pid, std::process::id());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(marker.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn same_host_live_marker_ignores_stale_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let marker = ReadMarker::create(dir.path(), "current").unwrap();
        filetime::set_file_mtime(marker.path(), filetime::FileTime::from_unix_time(0, 0)).unwrap();

        assert!(protected_read_marker_exists(dir.path(), "current"));
    }

    #[test]
    fn sweep_removes_dead_same_host_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker_path = read_marker_dir(dir.path(), "old").join("dead.json");
        let metadata = ReadMarkerMetadata {
            pid: 0,
            hostname: current_hostname(),
            created_at_ms: now_ms(),
            nonce: None,
        };
        fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
        write_marker_file(&marker_path, &metadata).unwrap();

        let sweep = sweep_read_markers(dir.path(), "old");

        assert!(!sweep.protected);
        assert_eq!(sweep.removed_stale, 1);
        assert!(!marker_path.exists());
    }

    #[test]
    fn sweep_removes_reused_pid_marker_when_created_at_predates_process_start() {
        let dir = tempfile::tempdir().unwrap();
        let marker_path = read_marker_dir(dir.path(), "old").join("reused.json");
        let pid = std::process::id();
        let metadata = ReadMarkerMetadata {
            pid,
            hostname: current_hostname(),
            created_at_ms: 1_000,
            nonce: None,
        };
        fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
        write_marker_file(&marker_path, &metadata).unwrap();
        set_process_start_time_for_test(pid, Some(10_000));

        let sweep = sweep_read_markers(dir.path(), "old");
        clear_process_start_time_for_test(pid);

        assert!(!sweep.protected);
        assert_eq!(sweep.removed_stale, 1);
        assert!(!marker_path.exists());
    }

    #[test]
    fn sweep_removes_expired_cross_host_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker_path = read_marker_dir(dir.path(), "old").join("cross-host.json");
        let metadata = ReadMarkerMetadata {
            pid: 123,
            hostname: format!("other-{}", current_hostname()),
            created_at_ms: now_ms(),
            nonce: None,
        };
        fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
        write_marker_file(&marker_path, &metadata).unwrap();
        let stale_time = SystemTime::now()
            .checked_sub(Duration::from_millis(
                READ_MARKER_CROSS_HOST_STALE_MS.saturating_add(1_000),
            ))
            .unwrap_or(UNIX_EPOCH);
        filetime::set_file_mtime(
            &marker_path,
            filetime::FileTime::from_system_time(stale_time),
        )
        .unwrap();

        let sweep = sweep_read_markers(dir.path(), "old");

        assert!(!sweep.protected);
        assert_eq!(sweep.removed_stale, 1);
        assert!(!marker_path.exists());
    }
}

#[cfg(test)]
mod residency_tests {
    use super::*;

    #[test]
    fn live_scope_marker_is_held_until_its_last_actor_leaves() {
        let storage = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let key = crate::path_identity::project_scope_key(project.path());
        register_live_scope(storage.path(), project.path());
        register_live_scope(storage.path(), project.path());
        let directory = read_marker_dir(&storage.path().join("retention"), &key);
        let names = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 1, "actors sharing a root share its marker");
        unregister_live_scope(storage.path(), project.path());
        assert!(names[0].exists(), "the other actor still holds the root");
        unregister_live_scope(storage.path(), project.path());
        assert!(!names[0].exists(), "the last actor releases the marker");
    }
}
