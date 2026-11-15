pub use aft_base::logging::*;

#[cfg(test)]
#[allow(unused_imports)]
use crate::executor::ExecutorContext;

#[cfg(test)]
#[allow(unused_imports)]
use sha2::Digest;
#[cfg(test)]
#[allow(unused_imports)]
use sha2::Sha256;
#[cfg(test)]
#[allow(unused_imports)]
use std::cell::RefCell;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::BTreeMap;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::HashMap;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::VecDeque;
#[cfg(test)]
#[allow(unused_imports)]
use std::fs;
#[cfg(test)]
#[allow(unused_imports)]
use std::fs::File;
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(test)]
#[allow(unused_imports)]
use std::io;
#[cfg(test)]
#[allow(unused_imports)]
use std::io::BufWriter;
#[cfg(test)]
#[allow(unused_imports)]
use std::io::Write;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::Path;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::PathBuf;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::AtomicBool;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::AtomicU64;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::Ordering;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::mpsc;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::mpsc::SyncSender;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::mpsc::TrySendError;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Arc;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::LazyLock;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Mutex;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::OnceLock;
#[cfg(test)]
#[allow(unused_imports)]
use std::thread;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::Duration;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::Instant;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::SystemTime;

#[cfg(test)]
#[allow(hidden_glob_reexports)]
mod tests {

    use super::*;

    #[allow(unused_imports)]
    pub(crate) use aft_base::logging::tests::*;

    #[test]
    fn semantic_cold_build_emits_started_progress_ready() {
        let (_temp, root) = tiny_project("sem", "lib.rs", "pub fn hello() {}\n");
        let key = crate::root_identity::key::artifact_cache_key(&root);
        let files = vec![root.join("lib.rs")];
        let mut embed = |texts: Vec<String>| {
            Ok(texts
                .into_iter()
                .map(|_| vec![0.01_f32; 384])
                .collect::<Vec<_>>())
        };
        let (built, lines) = capture_index_events(|| {
            crate::semantic_index::SemanticIndex::build(&root, &files, &mut embed, 8)
        });
        built.expect("semantic build");
        assert_lifecycle_sequence(&lines, "semantic", &normalize_index_root(&root), &key, true);
    }

    #[test]
    fn tier2_category_emits_started_ready() {
        let (_temp, root) = tiny_project("t2", "mod0.ts", "export function f0() { return 0; }\n");
        let key = crate::root_identity::key::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &key, false);
        let inspect_dir = _temp.path().join("inspect");
        let manager = std::sync::Arc::new(crate::inspect::InspectManager::new());
        let snapshot = crate::inspect::InspectSnapshot::new(
            root.clone(),
            inspect_dir,
            std::sync::Arc::new(crate::config::Config {
                project_root: Some(root.clone()),
                ..crate::config::Config::default()
            }),
            std::sync::Arc::new(std::sync::RwLock::new(crate::parser::SymbolCache::new())),
        );
        let (outcome, lines) = capture_index_events(|| {
            manager.tier2_run_with_reuse_blocking_fresh(
                snapshot,
                crate::inspect_category::InspectCategory::Complexity,
                crate::inspect::JobScope::for_project(root.clone()),
            )
        });
        assert!(outcome.payload().is_some(), "{outcome:?}");
        assert_lifecycle_sequence(&lines, "tier2", &normalize_index_root(&root), &key, false);
        assert!(
            !lines.iter().any(|line| {
                event_matches(line, "tier2", &normalize_index_root(&root), &key)
                    && line.contains("kind=build_progress")
            }),
            "tier2 must not emit synthetic build_progress: {lines:?}"
        );
    }
}
