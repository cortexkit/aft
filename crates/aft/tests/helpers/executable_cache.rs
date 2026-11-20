pub use aft_base::executable_cache::*;

#[cfg(test)]
#[allow(unused_imports)]
use sha2::{Digest, Sha256};
#[cfg(test)]
#[allow(unused_imports)]
use std::fs::{self, OpenOptions};
#[cfg(test)]
#[allow(unused_imports)]
use std::io::{self, Write};
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn executable_cache_refreshes_hits_and_prunes_on_publication() {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"#!/bin/sh\nexit 0\n";
        let mut usage = image_cache::Usage::default();
        usage.live_pids = Some([std::process::id()].into());
        let publish = |bytes: &[u8]| {
            publish_with_pruner(root.path(), bytes, "executable", |root, current, now| {
                image_cache::prune_with_usage(root, current, now, false, &usage)
            })
        };
        let old = publish(bytes).unwrap();
        let stale_time = SystemTime::now() - Duration::from_secs(25 * 60 * 60);
        let stale = filetime::FileTime::from_system_time(stale_time);
        filetime::set_file_mtime(old.parent().unwrap(), stale).unwrap();
        assert_eq!(publish(bytes).unwrap(), old);
        assert!(
            fs::metadata(old.parent().unwrap())
                .unwrap()
                .modified()
                .unwrap()
                > stale_time
        );
        let abandoned = root.path().join("f".repeat(64));
        fs::create_dir(&abandoned).unwrap();
        filetime::set_file_mtime(&abandoned, stale).unwrap();
        publish(b"another image").unwrap();
        assert!(old.exists());
        assert!(!abandoned.exists());
    }

    #[test]
    fn executable_cache_republishes_when_entry_disappears_before_linking() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("fixture");
        let cache = root.path().join("cache");
        let mut links = 0;
        let path = link_cached_executable_with(
            &destination,
            b"#!/bin/sh\nexit 0\n",
            |bytes| cached_executable_named_in(&cache, bytes, "executable"),
            |source, destination| {
                links += 1;
                if links == 1 {
                    fs::remove_dir_all(source.parent().unwrap())?;
                }
                link_cached_path(source, destination)
            },
        )
        .unwrap();
        assert_eq!(links, 2);
        assert_eq!(fs::read(&path).unwrap(), b"#!/bin/sh\nexit 0\n");
        #[cfg(unix)]
        assert!(std::process::Command::new(path).status().unwrap().success());
    }

    #[test]
    fn executable_cache_warms_once_at_final_owner_writable_path() {
        let root = tempfile::tempdir().unwrap();
        let warmed = std::cell::RefCell::new(Vec::new());
        let bytes = b"private shim image bytes";
        let daemon = root.path().join("daemon-image");
        fs::write(&daemon, bytes).unwrap();
        let image = cached_executable_named_in_with_warmup(root.path(), bytes, "aft", |path| {
            assert_eq!(fs::read(path).unwrap(), bytes);
            assert_eq!(path.file_name().unwrap(), "aft");
            assert_eq!(path.parent().unwrap().parent().unwrap(), root.path());
            assert!(!fs::metadata(path).unwrap().permissions().readonly());
            OpenOptions::new().write(true).open(path).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                assert_ne!(
                    fs::metadata(path).unwrap().ino(),
                    fs::metadata(&daemon).unwrap().ino(),
                    "a sandbox write regression must not corrupt the daemon image"
                );
            }
            warmed.borrow_mut().push(path.to_path_buf());
            Ok(())
        })
        .unwrap();
        assert_eq!(*warmed.borrow(), vec![image.clone()]);
        let cached = cached_executable_named_in_with_warmup(root.path(), bytes, "aft", |_| {
            panic!("a cached executable must not be warmed again")
        })
        .unwrap();
        assert_eq!(cached, image);
    }

    #[test]
    fn executable_cache_failed_warmup_is_not_reused() {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"private shim image bytes";
        let path = cache_path(root.path(), bytes, "aft");
        let attempts = std::cell::Cell::new(0);
        let error = cached_executable_named_in_with_warmup(root.path(), bytes, "aft", |_| {
            attempts.set(attempts.get() + 1);
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "injected warmup failure",
            ))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "injected warmup failure");
        assert_eq!(attempts.get(), 1, "warmup errors must not be retried");
        assert!(!path.exists(), "a failed warmup must not be cached");
        let warmed = std::cell::Cell::new(false);
        let retry = cached_executable_named_in_with_warmup(root.path(), bytes, "aft", |_| {
            warmed.set(true);
            Ok(())
        })
        .unwrap();
        assert_eq!(retry, path);
        assert!(warmed.get(), "the republished image must finish warmup");
    }
}
