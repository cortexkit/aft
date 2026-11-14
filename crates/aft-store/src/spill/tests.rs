use std::fs;
use std::io::{Seek, SeekFrom, Write};

use super::*;
use crate::test_rng::TestRng;

fn config(budget: usize) -> Config {
    Config {
        memory_budget: budget,
        buffer_bytes: 128,
        fan_in: 2,
    }
}

fn output(sorter: Sorter) -> Vec<(Vec<u8>, Vec<u8>)> {
    sorter
        .finish()
        .unwrap()
        .map(|r| {
            let r = r.unwrap();
            (r.key().to_vec(), r.value().to_vec())
        })
        .collect()
}

#[test]
fn spill_random_records_match_stable_std_sort_across_budgets() {
    for seed in 0..6 {
        let mut rng = TestRng::new(seed);
        let mut expected = Vec::new();
        for _ in 0..2300 {
            let key_len = rng.below_usize(13);
            let key = rng.bytes(key_len);
            let value_len = rng.below_usize(90);
            expected.push((key, rng.bytes(value_len)));
        }
        for budget in [12 * 1024, 24 * 1024, 1024 * 1024] {
            let root = tempfile::tempdir().unwrap();
            let mut sorter = Sorter::new(root.path(), config(budget)).unwrap();
            for (key, value) in &expected {
                sorter.push(key, value).unwrap();
            }
            let mut sorted = expected.clone();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let spilled = sorter.stats();
            if budget == 12 * 1024 {
                assert!(spilled.merge_passes > 2);
            }
            if budget == 1024 * 1024 {
                assert_eq!(spilled.runs_written, 0);
            }
            assert_eq!(output(sorter), sorted, "seed={seed}, budget={budget}");
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
        }
    }
}

#[test]
fn spill_prefix_keys_and_duplicates_have_defined_stable_order() {
    let root = tempfile::tempdir().unwrap();
    let mut sorter = Sorter::new(root.path(), config(12 * 1024)).unwrap();
    let keys: &[&[u8]] = &[b"ab\0", b"", b"a", b"ab", b"a\0", b"ab\0\0", b"ab", b"\xff"];
    let mut expected = Vec::new();
    for i in 0..5000u64 {
        let key = keys[(i % keys.len() as u64) as usize].to_vec();
        let value = i.to_be_bytes().to_vec();
        sorter.push(&key, &value).unwrap();
        expected.push((key, value));
    }
    assert!(sorter.stats().merge_passes > 0);
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(output(sorter), expected);
}

#[test]
fn spill_empty_and_single_record_need_no_runs() {
    let root = tempfile::tempdir().unwrap();
    let mut empty = Sorter::new(root.path(), config(12 * 1024))
        .unwrap()
        .finish()
        .unwrap();
    assert!(empty.next_record().unwrap().is_none());
    assert_eq!(empty.stats().runs_written, 0);
    let mut one = Sorter::new(root.path(), config(12 * 1024)).unwrap();
    one.push(b"", b"").unwrap();
    assert_eq!(output(one), vec![(vec![], vec![])]);
}

#[test]
fn spill_checkpoint_resumes_and_discards_unpublished_files() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(12 * 1024);
    let mut first = Sorter::new(root.path(), cfg).unwrap();
    for i in 0..1300u64 {
        first.push(b"duplicate", &i.to_be_bytes()).unwrap();
    }
    let saved = first.checkpoint().unwrap();
    assert!(saved.finished_runs > 0);
    let garbage = saved.directory.join("run-00000000000009999999.bin");
    crate::fs::write_new_durable(&garbage, b"unpublished").unwrap();
    let partial = saved.directory.join("manifest-99999999999999999999.part");
    crate::fs::write_new_durable(&partial, b"torn").unwrap();
    let mut resumed = Sorter::resume(&saved.directory, cfg).unwrap();
    let runs: Vec<_> = resumed.finished_runs().collect();
    assert_eq!(runs.len(), saved.finished_runs);
    assert_eq!(runs.iter().map(|run| run.records).sum::<u64>(), 1300);
    assert!(
        runs.iter()
            .all(|run| run.path.is_file() && run.max_record_bytes == 17)
    );
    assert!(!garbage.exists());
    assert!(!partial.exists());
    for i in 1300..2600u64 {
        resumed.push(b"duplicate", &i.to_be_bytes()).unwrap();
    }
    let actual = output(resumed);
    assert_eq!(actual.len(), 2600);
    for (i, record) in actual.iter().enumerate() {
        assert_eq!(record.1, (i as u64).to_be_bytes());
    }
    assert!(!saved.directory.exists());
}

fn saved_run(root: &Path, cfg: Config, count: u64) -> (Checkpoint, PathBuf) {
    let mut sorter = Sorter::new(root, cfg).unwrap();
    for i in 0..count {
        sorter.push(&i.to_be_bytes(), &[42; 80]).unwrap();
    }
    let checkpoint = sorter.checkpoint().unwrap();
    let run = fs::read_dir(&checkpoint.directory)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "bin"))
        .unwrap();
    (checkpoint, run)
}

#[test]
fn spill_corrupt_and_torn_runs_are_typed_errors_and_clean_up() {
    for mutation in [0, 1, 2] {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(24 * 1024);
        let (checkpoint, path) = saved_run(root.path(), cfg, 20);
        let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        match mutation {
            0 => {
                file.seek(SeekFrom::Start(80)).unwrap();
                file.write_all(&[0xff]).unwrap();
            }
            1 => {
                file.set_len(file.metadata().unwrap().len() - 1).unwrap();
            }
            _ => {
                file.seek(SeekFrom::Start(0)).unwrap();
                file.write_all(b"BADMAGIC").unwrap();
            }
        }
        drop(file);
        let sorter = Sorter::resume(&checkpoint.directory, cfg).unwrap();
        let err = sorter.finish().unwrap_err();
        assert!(matches!(err, Error::Corrupt { .. }), "{err:?}");
        assert!(!checkpoint.directory.exists());
    }
}

#[test]
fn spill_late_corruption_is_not_a_silent_partial_iterator() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(24 * 1024);
    let (checkpoint, path) = saved_run(root.path(), cfg, 30);
    let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::End(-1)).unwrap();
    file.write_all(&[0xff]).unwrap();
    drop(file);
    let mut records = Sorter::resume(&checkpoint.directory, cfg)
        .unwrap()
        .finish()
        .unwrap();
    let mut yielded = 0;
    loop {
        match records.next_record() {
            Ok(Some(_)) => yielded += 1,
            Err(Error::Corrupt { .. }) => break,
            other => panic!("expected corruption, got {other:?}"),
        }
    }
    assert!(yielded > 0 && yielded < 30);
    assert!(records.next_record().unwrap().is_none());
    assert!(!checkpoint.directory.exists());
}

#[test]
fn spill_manifest_corruption_is_not_ignored() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(24 * 1024);
    let (checkpoint, _) = saved_run(root.path(), cfg, 20);
    let manifest = fs::read_dir(&checkpoint.directory)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "ready"))
        .unwrap();
    let mut file = fs::OpenOptions::new().write(true).open(manifest).unwrap();
    file.seek(SeekFrom::End(-1)).unwrap();
    file.write_all(&[0xff]).unwrap();
    drop(file);
    assert!(matches!(
        Sorter::resume(&checkpoint.directory, cfg),
        Err(Error::Corrupt { .. })
    ));
    assert!(!checkpoint.directory.exists());
}

#[test]
fn spill_cleanup_on_drop_push_error_and_iterator_drop() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(12 * 1024);
    let mut sorter = Sorter::new(root.path(), cfg).unwrap();
    for i in 0..500u64 {
        sorter.push(&i.to_be_bytes(), b"value").unwrap();
    }
    let directory = sorter.directory().unwrap().to_owned();
    assert!(sorter.stats().disk_bytes > 0);
    drop(sorter);
    assert!(!directory.exists());
    let mut sorter = Sorter::new(root.path(), cfg).unwrap();
    let directory = sorter.directory().unwrap().to_owned();
    assert!(matches!(
        sorter.push(b"key", &vec![0; cfg.memory_budget]),
        Err(Error::Budget { .. })
    ));
    assert!(!directory.exists());
    assert!(matches!(sorter.push(b"key", b"value"), Err(Error::Failed)));
    let (checkpoint, _) = saved_run(root.path(), cfg, 500);
    let records = Sorter::resume(&checkpoint.directory, cfg)
        .unwrap()
        .finish()
        .unwrap();
    drop(records);
    assert!(!checkpoint.directory.exists());
}

#[test]
fn spill_write_error_cleans_partial_files() {
    let root = tempfile::tempdir().unwrap();
    let mut sorter = Sorter::new(root.path(), config(12 * 1024)).unwrap();
    sorter.push(b"key", b"value").unwrap();
    let directory = sorter.directory().unwrap().to_owned();
    // An exclusive-create collision forces a real write-path error on every OS.
    crate::fs::write_new_durable(
        &directory.join("run-00000000000000000000.part"),
        b"collision",
    )
    .unwrap();
    assert!(matches!(sorter.checkpoint(), Err(Error::Io(_))));
    assert!(!directory.exists());
}

#[test]
fn spill_merge_memory_is_capacity_counted_and_independent_of_output_count() {
    let root = tempfile::tempdir().unwrap();
    let cfg = Config {
        memory_budget: 48 * 1024,
        buffer_bytes: 256,
        fan_in: 4,
    };
    let mut sorter = Sorter::new(root.path(), cfg).unwrap();
    for i in (0..20_000u64).rev() {
        sorter
            .push(&(i % 41).to_be_bytes(), &i.to_be_bytes())
            .unwrap();
    }
    assert!(sorter.stats().runs_written > 20);
    assert!(sorter.stats().merge_passes > 0);
    let mut records = sorter.finish().unwrap();
    let before = records.stats();
    assert!(before.peak_merge_readers <= cfg.fan_in);
    assert_eq!(
        before.peak_merge_io_bytes,
        before.peak_merge_readers * cfg.buffer_bytes
    );
    assert_eq!(before.peak_merge_head_bytes, before.peak_merge_readers * 16);
    let mut count = 0;
    let mut previous = [0; 8];
    while let Some(record) = records.next_record().unwrap() {
        assert!(previous.as_slice() <= record.key);
        previous.copy_from_slice(record.key);
        count += 1;
        // Capacity counters cover all merge buffers and heads, even when empty.
        assert!(records.stats().buffered_bytes <= cfg.memory_budget);
    }
    let after = records.stats();
    assert_eq!(count, 20_000);
    assert_eq!(
        after.allocations, before.allocations,
        "lending merge must allocate nothing per record"
    );
    assert_eq!(after.buffered_bytes, 0);
    assert_eq!(after.disk_bytes, 0);
    assert!(after.peak_buffered_bytes <= cfg.memory_budget);
}

#[test]
fn spill_large_variable_records_cross_blocks_and_owned_outputs_fit() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(24 * 1024);
    let mut sorter = Sorter::new(root.path(), cfg).unwrap();
    sorter.push(b"large", &vec![3; 3000]).unwrap();
    let mut records = sorter.finish().unwrap();
    let record = records.next().unwrap().unwrap();
    assert_eq!(record.key(), b"large");
    assert_eq!(record.value(), vec![3; 3000]);
    assert!(records.next().is_none());
    assert!(records.stats().peak_buffered_bytes <= cfg.memory_budget);

    let mut sorter = Sorter::new(root.path(), cfg).unwrap();
    let mut expected = Vec::new();
    for i in (0..500u64).rev() {
        let value = vec![(i % 256) as u8; (i % 900 + 300) as usize];
        sorter.push(&i.to_be_bytes(), &value).unwrap();
        expected.push((i.to_be_bytes().to_vec(), value));
    }
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(output(sorter), expected);
}

#[test]
fn spill_insufficient_merge_budget_fails_instead_of_exceeding_limit() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(12 * 1024);
    let mut sorter = Sorter::new(root.path(), cfg).unwrap();
    // One large record fits the arena, but two heads plus readers do not fit
    // alongside metadata. Neither intermediate passes nor finish may overshoot.
    sorter.push(b"a", &vec![1; 3500]).unwrap();
    sorter.spill().unwrap();
    sorter.push(b"b", &vec![2; 3500]).unwrap();
    let directory = sorter.directory().unwrap().to_owned();
    assert!(matches!(sorter.finish(), Err(Error::Budget { .. })));
    assert!(!directory.exists());
}

#[test]
fn spill_sweep_requires_marked_directory_and_caller_ownership_decision() {
    let root = tempfile::tempdir().unwrap();
    let (dead, _) = saved_run(root.path(), config(12 * 1024), 500);
    let (retained, _) = saved_run(root.path(), config(12 * 1024), 500);
    let unmarked = root.path().join("aft-spill-not-ours");
    fs::create_dir(&unmarked).unwrap();
    assert_eq!(
        sweep_abandoned(root.path(), |p| p == dead.directory).unwrap(),
        1
    );
    assert!(!dead.directory.exists());
    assert!(retained.directory.exists());
    assert!(unmarked.exists());
    assert_eq!(sweep_abandoned(root.path(), |_| false).unwrap(), 0);
    drop(Sorter::resume(&retained.directory, config(12 * 1024)).unwrap());
}

#[cfg(unix)]
#[test]
fn spill_files_and_directory_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let (saved, _) = saved_run(root.path(), config(12 * 1024), 500);
    assert_eq!(
        fs::metadata(&saved.directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for entry in fs::read_dir(&saved.directory).unwrap() {
        assert_eq!(
            entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    drop(Sorter::resume(&saved.directory, config(12 * 1024)).unwrap());
}

#[test]
fn spill_capacity_counter_refuses_allocation_before_budget_is_exceeded() {
    let memory = Memory::new(1000);
    let first = Buffer::<u64>::new(100, &memory).unwrap();
    assert_eq!(memory.stats().buffered_bytes, first.capacity() * 8);
    assert!(matches!(
        Buffer::<u8>::new(201, &memory),
        Err(Error::Budget { .. })
    ));
    assert_eq!(memory.stats().allocations, 1);
    drop(first);
    assert_eq!(memory.stats().buffered_bytes, 0);
}

/// Run explicitly in release mode on the build server. Generates input without
/// storing it, verifies output count/order, and includes durable writes in time.
#[test]
#[ignore = "50 million record throughput measurement; run explicitly in release mode"]
fn spill_throughput_50m_24byte_256mib() {
    let root = tempfile::tempdir().unwrap();
    let cfg = Config::new(256 * 1024 * 1024);
    let mut sorter = Sorter::new(root.path(), cfg).unwrap();
    let started = std::time::Instant::now();
    let mut rng = TestRng::new(73);
    const COUNT: u64 = 50_000_000;
    for i in 0..COUNT {
        let key = rng.next_u64().to_be_bytes();
        let mut value = [0; 16];
        value[..8].copy_from_slice(&i.to_be_bytes());
        value[8..].copy_from_slice(&(!i).to_be_bytes());
        sorter.push(&key, &value).unwrap();
    }
    let push_seconds = started.elapsed().as_secs_f64();
    let mut records = sorter.finish().unwrap();
    let mut count = 0;
    let mut previous = [0; 8];
    while let Some(record) = records.next_record().unwrap() {
        assert!(previous.as_slice() <= record.key);
        previous.copy_from_slice(record.key);
        assert_eq!(record.value.len(), 16);
        count += 1;
    }
    assert_eq!(count, COUNT);
    let seconds = started.elapsed().as_secs_f64();
    let stats = records.stats();
    assert!(stats.peak_buffered_bytes <= cfg.memory_budget);
    println!(
        "spill benchmark: records={COUNT} input_bytes={} push_seconds={push_seconds:.3} total_seconds={seconds:.3} records_per_second={:.0} input_mib_per_second={:.2} peak_buffered_bytes={} peak_disk_bytes={} bytes_written={} runs={} merge_passes={} peak_readers={}",
        COUNT * 24,
        COUNT as f64 / seconds,
        COUNT as f64 * 24.0 / 1048576.0 / seconds,
        stats.peak_buffered_bytes,
        stats.peak_disk_bytes,
        stats.bytes_written,
        stats.runs_written,
        stats.merge_passes,
        stats.peak_merge_readers
    );
}
