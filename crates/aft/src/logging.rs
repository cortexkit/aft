pub use aft_base::logging::*;

#[cfg(test)]
#[allow(unused_imports)]
use crate::executor::{Executor, ExecutorContext};
#[cfg(test)]
#[allow(unused_imports)]
use crate::process_identity::is_process_alive;
#[cfg(test)]
#[allow(unused_imports)]
use crate::tool_progress::{ToolCallPhaseDurations, WaitingOn};
#[cfg(test)]
#[allow(unused_imports)]
use sha2::{Digest, Sha256};
#[cfg(test)]
#[allow(unused_imports)]
use std::cell::RefCell;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::{BTreeMap, HashMap, VecDeque};
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(test)]
#[allow(unused_imports)]
use std::fs::{self, File};
#[cfg(test)]
#[allow(unused_imports)]
use std::io::{self, BufWriter, Write};
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::mpsc::{self, SyncSender, TrySendError};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
#[cfg(test)]
#[allow(unused_imports)]
use std::thread;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::{Duration, Instant, SystemTime};

#[cfg(test)]
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

#[cfg(test)]
mod stderr_mirror;
