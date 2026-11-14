pub use aft_base::cache_location::*;

#[cfg(test)]
#[allow(unused_imports)]
use crate::pinned_dir::{PinnedDir, RepairBudget};
#[cfg(test)]
#[allow(unused_imports)]
use serde::{Deserialize, Serialize};
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::{HashMap, HashSet};
#[cfg(test)]
#[allow(unused_imports)]
use std::fs::{self, File};
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
use std::sync::{Arc, Mutex, OnceLock, Weak};
#[cfg(test)]
#[allow(unused_imports)]
use std::time::Duration;

#[cfg(test)]
#[path = "cache_location_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "gh_shim_speech/serving_tests.rs"]
mod speech_serving_tests;

#[cfg(test)]
mod prune_legacy_tests {

    use super::*;

    #[allow(unused_imports)]
    pub(crate) use aft_base::cache_location::prune_legacy_tests::*;

    #[test]
    fn prune_legacy_also_cleans_the_enrolled_external_cache_root() {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        // Plain spelling on Windows, as the command reports it.
        #[cfg(windows)]
        let base = crate::windows_path::normalize_windows_path(&base);
        let storage = base.join("storage");
        let parent = base.join("cache-volume");
        fs::create_dir(&parent).unwrap();
        let mut config = crate::config::Config {
            storage_dir: Some(storage.clone()),
            cache_dir: Some(parent.clone()),
            ..Default::default()
        };
        let warning = crate::reader_floor::prepare_cache_root_for_test(&mut config);
        assert!(warning.is_none(), "{warning:?}");
        drop(config);
        fs::write(storage.join("aft.db"), "db").unwrap();
        let cache = parent.join("aft");
        for (relative, contents) in [
            ("views/a", "old"),
            ("index/key-1/cache.bin", "old"),
            ("semantic/key-1/semantic.bin", "old"),
            ("semantic/models/model.onnx", "model"),
            ("data/keep", "new"),
        ] {
            let path = cache.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        let tiers = [crate::config_resolve::ConfigTier {
            tier: "user".into(),
            source: "test".into(),
            doc: serde_json::json!({"storage": {"cache_dir": parent}}).to_string(),
        }];
        let dry = run_with_tiers(&tiers, &storage, false);
        assert_eq!(dry.roots, vec![storage.clone(), cache.clone()]);
        // Only the cache root's part: preparing the cache root above may
        // leave its own folders under the storage root.
        let in_cache = |stores: &[LegacyStore]| {
            found_paths(stores)
                .into_iter()
                .filter(|path| path.starts_with(&cache))
                .collect::<Vec<_>>()
        };
        let expected = ["index", "semantic/key-1", "views"].map(|name| cache.join(name));
        assert_eq!(in_cache(&dry.stores), expected);
        let report = run_with_tiers(&tiers, &storage, true);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(in_cache(&report.removed), expected);
        for path in expected {
            assert!(!path.exists(), "{}", path.display());
        }
        assert_eq!(fs::read_to_string(cache.join("data/keep")).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(cache.join("semantic/models/model.onnx")).unwrap(),
            "model"
        );
        assert!(cache.join(OWNER).is_file());
        assert!(asides(&cache).is_empty());
    }
}

#[cfg(test)]
mod markers;
