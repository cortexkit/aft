pub use aft_base::engram_catalog::*;

#[cfg(test)]
#[allow(unused_imports)]
use serde::Deserialize;
#[cfg(test)]
#[allow(unused_imports)]
use serde::Serialize;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::BTreeMap;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::BTreeSet;
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
#[allow(hidden_glob_reexports)]
mod tests {

    #[allow(unused_imports)]
    pub(crate) use aft_base::engram_catalog::tests::*;

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
}
