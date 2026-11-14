pub mod profile;
pub use aft_base::memory::*;
#[cfg(test)]
use std::collections::BTreeMap;

#[cfg(test)]
#[allow(unused_imports)]
use serde::Serialize;
#[cfg(test)]
#[allow(unused_imports)]
use serde_json::Value;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::BTreeMap;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::Path;

#[cfg(test)]
mod snapshot_cap_tests {
    use super::*;

    fn root_with_bytes(bytes: u64) -> RootMemorySnapshot {
        let estimate = MemoryEstimate::estimated;
        RootMemorySnapshot::new(
            estimate(bytes),
            estimate(0),
            estimate(0),
            estimate(0),
            estimate(0),
            estimate(0),
            estimate(0),
            estimate(0),
            estimate(0),
        )
    }

    #[test]
    fn detail_map_capped_but_totals_cover_all_roots() {
        let mut roots = BTreeMap::new();
        for i in 0..(MEMORY_SNAPSHOT_ROOT_DETAIL_CAP + 4) {
            // Distinct sizes so the kept set is deterministic: later roots larger.
            roots.insert(
                format!("/root/{i:02}"),
                root_with_bytes((i as u64 + 1) * 1000),
            );
        }
        let snapshot = crate::memory_census::snapshot("ready", roots);

        assert_eq!(snapshot.roots.len(), MEMORY_SNAPSHOT_ROOT_DETAIL_CAP);
        assert_eq!(snapshot.roots_total, MEMORY_SNAPSHOT_ROOT_DETAIL_CAP + 4);
        assert_eq!(snapshot.roots_omitted, 4);
        // The four smallest (1000..=4000) are the omitted ones.
        assert_eq!(snapshot.roots_omitted_bytes, 1000 + 2000 + 3000 + 4000);
        // Largest roots are the ones kept.
        assert!(snapshot
            .roots
            .values()
            .all(|root| root.attributed_bytes > 4000));
        // Process totals include omitted roots' bytes.
        let expected_total: u64 = (1..=(MEMORY_SNAPSHOT_ROOT_DETAIL_CAP as u64 + 4))
            .map(|i| i * 1000)
            .sum();
        assert!(snapshot.process.total_attributed_bytes >= expected_total);
    }

    #[test]
    fn rollup_cap_keeps_only_top_roots_while_totals_cover_all() {
        let mut roots = BTreeMap::new();
        for i in 0..(MEMORY_SNAPSHOT_ROOT_DETAIL_CAP + 4) {
            roots.insert(
                format!("/rollup/{i:02}"),
                root_with_bytes((i as u64 + 1) * 1000).rollup(),
            );
        }
        let snapshot =
            crate::memory_census::rollup_snapshot("ready", roots, AllocatorObservation::measure());

        assert_eq!(snapshot.roots.len(), MEMORY_SNAPSHOT_ROOT_DETAIL_CAP);
        assert_eq!(snapshot.roots_total, MEMORY_SNAPSHOT_ROOT_DETAIL_CAP + 4);
        assert_eq!(snapshot.roots_omitted, 4);
        assert_eq!(snapshot.roots_omitted_bytes, 1000 + 2000 + 3000 + 4000);
        let expected_total: u64 = (1..=(MEMORY_SNAPSHOT_ROOT_DETAIL_CAP as u64 + 4))
            .map(|i| i * 1000)
            .sum();
        assert!(snapshot.process.total_attributed_bytes >= expected_total);
    }

    #[test]
    fn standing_rollup_uses_the_shared_top_eight_cap() {
        let mut roots = BTreeMap::new();
        for i in 0..=MEMORY_SNAPSHOT_ROOT_DETAIL_CAP {
            let rollup = root_with_bytes((i as u64 + 1) * 1000).rollup();
            roots.insert(format!("/root/{i:02}"), rollup);
        }
        roots.insert(
            "standing-artifact-key".to_string(),
            root_with_bytes(20_000).rollup().with_standing(),
        );

        let snapshot =
            crate::memory_census::rollup_snapshot("ready", roots, AllocatorObservation::measure());
        assert_eq!(snapshot.roots.len(), MEMORY_SNAPSHOT_ROOT_DETAIL_CAP);
        assert_eq!(snapshot.roots_total, MEMORY_SNAPSHOT_ROOT_DETAIL_CAP + 2);
        assert_eq!(snapshot.roots_omitted, 2);
        assert_eq!(
            snapshot.roots["standing-artifact-key"].standing,
            Some(true),
            "standing memory stays in the ordinary per-root table"
        );
    }

    #[test]
    fn under_cap_keeps_everything_with_zero_omitted() {
        let mut roots = BTreeMap::new();
        roots.insert("/a".to_string(), root_with_bytes(10));
        roots.insert("/b".to_string(), root_with_bytes(20));
        let snapshot = crate::memory_census::snapshot("ready", roots);
        assert_eq!(snapshot.roots.len(), 2);
        assert_eq!(snapshot.roots_total, 2);
        assert_eq!(snapshot.roots_omitted, 0);
        assert_eq!(snapshot.roots_omitted_bytes, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_snapshot_preserves_negative_residuals() {
        assert_eq!(signed_difference(5, 8), -3);
    }

    #[test]
    fn allocator_reuse_window_stretches_with_walk_cost() {
        // A cheap walk keeps the floor.
        assert_eq!(allocator_reuse_window_ms(30_000, 0), 30_000);
        assert_eq!(allocator_reuse_window_ms(30_000, 150), 30_000);
        // An expensive walk waits 200 times its own duration...
        assert_eq!(allocator_reuse_window_ms(30_000, 1_000), 200_000);
        // ...but never more than ten minutes.
        assert_eq!(allocator_reuse_window_ms(30_000, 6_500), 600_000);
        assert_eq!(allocator_reuse_window_ms(30_000, u64::MAX), 600_000);
        // A floor above the cap still wins.
        assert_eq!(allocator_reuse_window_ms(900_000, 6_500), 900_000);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn slack_sample_waits_longer_after_an_expensive_walk() {
        assert!(sample_is_stale_after(UNSAMPLED_AT_MS, 0, 0));
        // A cheap walk: due once a minute.
        assert!(!sample_is_stale_after(0, 59_999, 20));
        assert!(sample_is_stale_after(0, 60_000, 20));
        // A one-second walk: due after 200 seconds, not after one minute.
        assert!(!sample_is_stale_after(0, 60_000, 1_000));
        assert!(!sample_is_stale_after(0, 199_999, 1_000));
        assert!(sample_is_stale_after(0, 200_000, 1_000));
        // A multi-second glibc walk: capped at ten minutes.
        assert!(!sample_is_stale_after(0, 599_999, 6_500));
        assert!(sample_is_stale_after(0, 600_000, 6_500));
    }

    #[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
    #[test]
    fn cached_allocator_before_first_publication_is_unavailable_not_unsupported() {
        let _test_guard = allocator_observation_test_lock();
        let publish_guard = ALLOCATOR_OBSERVATION_PUBLISH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_allocator_observation_for_test();
        let calls_before = allocator_snapshot_calls_for_test();

        // Hold publication at startup: a request can arrive before the sampler
        // publishes its first reading, even on a measurable allocator.
        let (allocator, age_ms) = allocator_observation(AllocatorSource::Cached);
        assert_eq!(allocator.status, "unavailable");
        assert_eq!(
            allocator.not_estimated,
            Some("allocator_observation_unavailable")
        );
        assert_eq!(allocator.bytes_in_use, None);
        assert_eq!(allocator.size_allocated, None);
        assert_eq!(allocator.retained_slack_bytes, None);
        assert_eq!(age_ms, None);
        assert_eq!(allocator_snapshot_calls_for_test(), calls_before);

        drop(publish_guard);
        publish_allocator_slack(&measured_allocator(100, 140), 0);
        let (allocator, age_ms) = allocator_observation(AllocatorSource::Cached);
        assert_eq!(allocator.status, "measured");
        assert_eq!(allocator.not_estimated, None);
        assert_eq!(allocator.bytes_in_use, Some(100));
        assert_eq!(allocator.retained_slack_bytes, Some(40));
        assert!(age_ms.is_some());
        assert_eq!(allocator_snapshot_calls_for_test(), calls_before);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn loop_slack_decision_never_reads_allocator_statistics() {
        LAST_OBSERVED_ALLOCATOR_SLACK_BYTES.store(
            UNKNOWN_ALLOCATOR_SLACK,
            std::sync::atomic::Ordering::Release,
        );
        LAST_ALLOCATOR_SLACK_SAMPLE_AT_MS
            .store(UNSAMPLED_AT_MS, std::sync::atomic::Ordering::Release);
        LAST_ALLOCATOR_SLACK_RELIEF_AT_MS
            .store(UNSAMPLED_AT_MS, std::sync::atomic::Ordering::Release);
        ALLOCATOR_SNAPSHOT_THREADS
            .lock()
            .expect("clear allocator snapshot thread log")
            .clear();

        for _ in 0..1_000 {
            assert!(!spawn_allocator_slack_relief_if_due(
                std::time::Instant::now()
            ));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let sampler_ran = || {
            ALLOCATOR_SNAPSHOT_THREADS
                .lock()
                .expect("read allocator snapshot thread log")
                .iter()
                .any(|name| name == "aft-mem-slack-sample")
        };
        while !sampler_ran() {
            assert!(std::time::Instant::now() < deadline, "sampler did not run");
            std::thread::yield_now();
        }
        // The thread log is process-global and other tests in the parallel suite
        // read allocator statistics on their own threads; the property under
        // test is only that THIS thread (the loop stand-in) never did.
        let loop_thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();
        let threads = ALLOCATOR_SNAPSHOT_THREADS
            .lock()
            .expect("read allocator snapshot thread log")
            .clone();
        assert!(
            !threads.iter().any(|name| *name == loop_thread),
            "the loop-side decision must never read allocator statistics: {threads:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_automatic_slack_sampling_and_relief_are_explicit_only() {
        let before = allocator_snapshot_calls_for_test();
        for seconds in [0, 30, 300, 3_600] {
            assert!(!spawn_allocator_slack_relief_if_due(
                std::time::Instant::now() + std::time::Duration::from_secs(seconds)
            ));
        }
        assert_eq!(allocator_snapshot_calls_for_test(), before);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn slack_relief_fires_on_large_slack_and_respects_spacing() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        let big = Some(ALLOCATOR_SLACK_RELIEF_THRESHOLD_BYTES);
        // Unknown slack (allocator stats unavailable) never fires.
        assert!(!allocator_slack_relief_due(None, None, now));
        // Below threshold never fires.
        assert!(!allocator_slack_relief_due(
            Some(ALLOCATOR_SLACK_RELIEF_THRESHOLD_BYTES - 1),
            None,
            now
        ));
        // At threshold with no prior pass fires.
        assert!(allocator_slack_relief_due(big, None, now));
        // A recent pass suppresses the next one...
        let recent = now - Duration::from_secs(10);
        assert!(!allocator_slack_relief_due(big, Some(recent), now));
        // ...until the minimum spacing has elapsed.
        let stale = now - ALLOCATOR_SLACK_RELIEF_MIN_INTERVAL;
        assert!(allocator_slack_relief_due(big, Some(stale), now));
    }

    #[test]
    fn json_estimator_scales_with_payload_content() {
        let empty = estimated_json_bytes(&serde_json::json!({}));
        let populated = estimated_json_bytes(&serde_json::json!({"message": "hello"}));
        assert_eq!(empty, 0);
        assert!(populated >= 12);
    }

    #[test]
    fn process_snapshot_exposes_sqlite_and_allocator_sections() {
        let shared = MemoryEstimate::estimated(7);
        let snapshot = crate::memory_census::process_from_roots(&BTreeMap::new(), &shared);
        assert_eq!(snapshot.sqlite.status, "measured");
        assert!(snapshot.sqlite.memory_highwater_bytes >= snapshot.sqlite.memory_used_bytes);
        // The data layer's block cache is counted once for the process when
        // a test of this process has bound a checkout.
        assert_eq!(
            snapshot.total_attributed_bytes,
            snapshot
                .sqlite
                .memory_used_bytes
                .saturating_add(7)
                .saturating_add(snapshot.data_layer_block_cache_bytes.unwrap_or(0))
        );

        let serialized = serde_json::to_value(&snapshot).expect("serialize process memory");
        assert!(serialized["sqlite"]["memory_used_bytes"].is_u64());
        assert!(serialized["allocator"].get("bytes_in_use").is_some());
        assert!(serialized["allocator"].get("size_allocated").is_some());
        assert!(serialized["allocator"]
            .get("retained_slack_bytes")
            .is_some());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn allocator_snapshot_reports_measured_slack() {
        let allocator = allocator_memory_snapshot();
        if allocator.status == "measured" {
            let in_use = allocator.bytes_in_use.expect("allocator bytes in use");
            let allocated = allocator.size_allocated.expect("allocator size allocated");
            assert_eq!(
                allocator.retained_slack_bytes,
                Some(allocated.saturating_sub(in_use))
            );
        } else {
            assert_eq!(allocator.status, "not_estimated_on_this_platform");
            assert_eq!(allocator.bytes_in_use, None);
            assert_eq!(allocator.size_allocated, None);
            assert_eq!(allocator.retained_slack_bytes, None);
            assert_eq!(
                allocator.not_estimated,
                Some("jemalloc_statistics_unavailable")
            );
        }
    }

    fn measured_allocator(bytes_in_use: u64, size_allocated: u64) -> AllocatorMemorySnapshot {
        AllocatorMemorySnapshot {
            status: "measured",
            bytes_in_use: Some(bytes_in_use),
            size_allocated: Some(size_allocated),
            retained_slack_bytes: Some(size_allocated.saturating_sub(bytes_in_use)),
            retained_slack_label: ALLOCATOR_SLACK_LABEL,
            not_estimated: None,
            #[cfg(target_os = "linux")]
            jemalloc: Some(JemallocMemorySnapshot {
                accounting_source: "jemalloc",
                allocated: bytes_in_use,
                active: bytes_in_use,
                resident: size_allocated,
                retained: 0,
            }),
        }
    }

    #[cfg(target_os = "linux")]
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    /// Kernel RSS can fall independently of the allocator's resident estimate.
    /// The record must carry both figures, each labelled.
    #[cfg(target_os = "linux")]
    #[test]
    fn relief_reports_rss_drop_when_allocator_accounting_is_unchanged() {
        let in_use = 2 * GIB;
        let size_allocated = in_use + 7560 * MIB;
        let before = ResidentReading {
            rss_bytes: Some(6440 * MIB),
            phys_footprint_bytes: None,
        };
        let after = ResidentReading {
            rss_bytes: Some(4910 * MIB),
            phys_footprint_bytes: None,
        };
        let relief = AllocatorPressureRelief::from_jemalloc_resident_drop(
            before,
            after,
            measured_allocator(in_use, size_allocated),
            measured_allocator(in_use, size_allocated),
        );
        assert_eq!(relief.allocator_accounting_bytes, 0);
        assert_eq!(
            relief.allocator_accounting_source,
            RELIEF_ACCOUNTING_SOURCE_JEMALLOC
        );
        assert_eq!(relief.rss_drop_bytes, Some(1530 * MIB));
        assert_eq!(relief.rss_before_bytes, Some(6440 * MIB));
        assert_eq!(relief.rss_after_bytes, Some(4910 * MIB));
        assert_eq!(relief.phys_footprint_drop_bytes, None);
        // Slack is unchanged even though memory went back to the OS: it is
        // address space, not residency.
        assert_eq!(
            relief.allocator_before.retained_slack_bytes,
            relief.allocator_after.retained_slack_bytes
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn relief_keeps_allocator_accounting_and_resident_drops_independent() {
        // jemalloc's resident estimate fell by 64 MiB while RSS fell by 300 MiB.
        let relief = AllocatorPressureRelief::from_jemalloc_resident_drop(
            ResidentReading {
                rss_bytes: Some(1000 * MIB),
                phys_footprint_bytes: None,
            },
            ResidentReading {
                rss_bytes: Some(700 * MIB),
                phys_footprint_bytes: None,
            },
            measured_allocator(100 * MIB, 500 * MIB),
            measured_allocator(100 * MIB, 436 * MIB),
        );
        assert_eq!(relief.allocator_accounting_bytes, 64 * MIB);
        assert_eq!(relief.rss_drop_bytes, Some(300 * MIB));

        // RSS growth during the pass floors the drop at zero; a missing
        // reading yields no drop rather than a guessed one.
        let grew = AllocatorPressureRelief::from_jemalloc_resident_drop(
            ResidentReading {
                rss_bytes: Some(700 * MIB),
                phys_footprint_bytes: Some(500 * MIB),
            },
            ResidentReading {
                rss_bytes: Some(900 * MIB),
                phys_footprint_bytes: None,
            },
            measured_allocator(100 * MIB, 500 * MIB),
            measured_allocator(100 * MIB, 500 * MIB),
        );
        assert_eq!(grew.rss_drop_bytes, Some(0));
        assert_eq!(grew.phys_footprint_drop_bytes, None);
    }

    #[test]
    fn macos_style_relief_reports_allocator_return_and_footprint_drop() {
        let relief = AllocatorPressureRelief::from_observations(
            40 * MIB,
            RELIEF_ACCOUNTING_SOURCE_MACOS,
            ResidentReading {
                rss_bytes: Some(900 * MIB),
                phys_footprint_bytes: Some(600 * MIB),
            },
            ResidentReading {
                rss_bytes: Some(900 * MIB),
                phys_footprint_bytes: Some(520 * MIB),
            },
            measured_allocator(100 * MIB, 500 * MIB),
            measured_allocator(100 * MIB, 500 * MIB),
        );
        assert_eq!(relief.allocator_accounting_bytes, 40 * MIB);
        assert_eq!(
            relief.allocator_accounting_source,
            RELIEF_ACCOUNTING_SOURCE_MACOS
        );
        assert_eq!(relief.rss_drop_bytes, Some(0));
        assert_eq!(relief.phys_footprint_drop_bytes, Some(80 * MIB));
    }

    #[test]
    fn allocator_snapshot_labels_slack_as_address_space() {
        let value = serde_json::to_value(measured_allocator(1, 3)).unwrap();
        assert_eq!(value["retained_slack_bytes"], 2);
        assert_eq!(value["retained_slack_label"], ALLOCATOR_SLACK_LABEL);
        assert!(ALLOCATOR_SLACK_LABEL.starts_with("address-space slack"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_allocator_pressure_relief_smoke() {
        let mut allocation = vec![0u8; 32 * 1024 * 1024];
        for byte in allocation.iter_mut().step_by(4096) {
            *byte = 1;
        }
        std::hint::black_box(&allocation);
        drop(allocation);

        let relief = relieve_allocator_pressure();
        assert_eq!(
            relief.allocator_accounting_source,
            RELIEF_ACCOUNTING_SOURCE_JEMALLOC
        );
        assert!(relief.allocator_before.jemalloc.is_some());
        assert!(relief.allocator_after.jemalloc.is_some());
        purge_allocator().expect("jemalloc all-arena purge must succeed");
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[test]
    fn allocator_snapshot_is_honest_when_platform_counters_are_unavailable() {
        let allocator = allocator_memory_snapshot();
        assert_eq!(allocator.status, "not_estimated_on_this_platform");
        assert_eq!(allocator.bytes_in_use, None);
        assert_eq!(allocator.size_allocated, None);
        assert_eq!(allocator.retained_slack_bytes, None);
        assert_eq!(
            allocator.not_estimated,
            Some("platform_allocator_statistics_unavailable")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "bounded live RSS experiment; run explicitly after allocator changes"]
    fn allocator_pressure_relief_warm_then_idle_measurement() {
        let warm_pages = (0..16 * 1024)
            .map(|seed| {
                let mut page = Box::new([0u8; 4096]);
                page[0] = seed as u8;
                page
            })
            .collect::<Vec<_>>();
        std::hint::black_box(&warm_pages);
        drop(warm_pages);

        let relief = relieve_allocator_pressure();
        let sqlite = SqliteMemorySnapshot::measure();
        eprintln!(
            "warm-then-idle pressure relief: rss_before={:?} rss_after={:?} footprint_before={:?} footprint_after={:?} allocator_in_use_before={:?} allocator_in_use_after={:?} allocator_allocated_before={:?} allocator_allocated_after={:?} address_space_slack_before={:?} address_space_slack_after={:?} allocator_accounting={} ({}) sqlite_used={} sqlite_highwater={}",
            relief.rss_before_bytes,
            relief.rss_after_bytes,
            relief.phys_footprint_before_bytes,
            relief.phys_footprint_after_bytes,
            relief.allocator_before.bytes_in_use,
            relief.allocator_after.bytes_in_use,
            relief.allocator_before.size_allocated,
            relief.allocator_after.size_allocated,
            relief.allocator_before.retained_slack_bytes,
            relief.allocator_after.retained_slack_bytes,
            relief.allocator_accounting_bytes,
            relief.allocator_accounting_source,
            sqlite.memory_used_bytes,
            sqlite.memory_highwater_bytes,
        );
        assert_eq!(relief.allocator_before.status, "measured");
        assert_eq!(relief.allocator_after.status, "measured");
    }
}
