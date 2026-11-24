#![cfg_attr(test, allow(hidden_glob_reexports))]
pub use aft_db::write_ledger::*;

#[cfg(test)]
#[allow(unused_imports)]
use crate::process_io::Bytes;
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(test)]
use crate::write_counters::now_ms;
#[cfg(test)]
#[allow(unused_imports)]
pub use crate::write_counters::{
    credit, note_process_unmeasurable, process_root_counter, register, Counter, Domain, WriterRow,
    DEFAULT_WINDOW_MS, RETENTION_MS,
};
#[cfg(test)]
#[allow(unused_imports)]
use crate::write_counters::{
    registry, saturating_add, Entry, ProcessBaseline, RecentRow, UnmeasurableEntry, MINUTE_MS,
};
#[cfg(test)]
#[allow(unused_imports)]
use serde::{Deserialize, Serialize};
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Arc;
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(test)]
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex, OnceLock,
};

#[cfg(test)]
mod tests {
    use aft_db::write_ledger::tests::*;

    use super::*;

    #[test]
    fn inspect_tier2_writes_land_in_inspect_cache_not_unexplained() {
        let _guard = test_lock();
        let dir = tempfile::tempdir().unwrap();
        let mut ledger_conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let root = project.display().to_string();
        let cache = crate::inspect::InspectCache::open(dir.path().join("inspect"), project.clone())
            .unwrap();
        let page_size = cache.page_size_for_test();
        // WAL growth is read with a metadata stat only: opening another
        // descriptor on a live SQLite file set would drop this process's
        // advisory locks.
        let wal_path = std::path::PathBuf::from(format!("{}-wal", cache.sqlite_path().display()));
        let wal_frames = |path: &std::path::Path| {
            std::fs::metadata(path)
                .map(|metadata| metadata.len().saturating_sub(32) / (page_size + 24))
                .unwrap_or(0)
        };
        let frames_before = wal_frames(&wal_path);
        let main_path = cache.sqlite_path().to_path_buf();
        let main_len = |path: &std::path::Path| std::fs::metadata(path).map_or(0, |m| m.len());
        let main_before = main_len(&main_path);
        let credited_before = pending_for_test(Domain::InspectCache, &root).1;
        // A fresh cache has written its main file (SQLite writes it directly
        // while setting the database up) plus the schema frames in the WAL;
        // both belong to the same census window as the Tier-2 run below.
        let opened = main_before + frames_before.saturating_mul(page_size);
        assert_eq!(
            credited_before, opened,
            "schema pages written at open must be credited"
        );

        // The same two writes a background Tier-2 run makes: per-file
        // contributions, then the aggregate over them.
        let mut upserts = Vec::new();
        for index in 0..24 {
            let source = project.join(format!("src_{index}.ts"));
            std::fs::write(&source, format!("export const value{index} = {index};\n")).unwrap();
            upserts.push(crate::inspect::job::FileContribution::new(
                crate::inspect_category::InspectCategory::DeadCode,
                source.clone(),
                crate::cache_freshness::collect(&source).unwrap(),
                serde_json::json!({
                    "file": format!("src_{index}.ts"),
                    "exports": [{ "symbol": format!("value{index}"), "kind": "const", "line": 1 }],
                    "padding": "x".repeat(2048),
                }),
            ));
        }
        let hash = crate::inspect::compatibility_test::apply_contribution_upserts(
            &cache,
            crate::inspect_category::InspectCategory::DeadCode,
            upserts,
            &crate::config::Config::default(),
        )
        .unwrap();
        use crate::test_bridge::search_misc::InspectCacheTestExt as _;
        cache
            .store_tier2_aggregate(
                crate::inspect::job::JobKey::for_project_category(
                    crate::inspect_category::InspectCategory::DeadCode,
                ),
                &hash,
                serde_json::json!({ "count": 0, "items": [], "padding": "y".repeat(4096) }),
            )
            .unwrap();

        let written = wal_frames(&wal_path)
            .saturating_sub(frames_before)
            .saturating_mul(page_size)
            + main_len(&main_path).saturating_sub(main_before);
        assert!(written > 0, "the fixture did not write WAL frames");
        let credited = pending_for_test(Domain::InspectCache, &root)
            .1
            .saturating_sub(credited_before);
        assert_eq!(
            credited, written,
            "Tier-2 inspect writes must be credited when they commit, not when the cache closes"
        );

        let report = census_after_process_wrote(&mut ledger_conn, &root, opened + written);
        assert_eq!(report.attributed_physical_bytes, opened + written);
        assert_eq!(report.unexplained_physical_bytes, Some(0));
        drop(cache);
    }
}
