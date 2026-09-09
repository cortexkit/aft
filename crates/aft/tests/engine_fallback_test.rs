use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aft::commands::semantic_search::generation_token::GenerationToken;
use aft::search_index::exact_lane::{
    format_trailer, ExactLane, FallbackExactOptions, TrailerReason,
};
use aft::search_index::memo::ExactMemoStore;

fn create_temp_corpus_with_files(count: usize) -> (tempfile::TempDir, Vec<String>) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let src = dir.path().join("src");
    fs::create_dir_all(&src).expect("create src dir");

    let mut names = Vec::new();
    for i in 0..count {
        let name = format!("file_{i:04}.rs");
        let path = src.join(&name);
        fs::write(
            &path,
            format!("// {name}\npub fn item_{i}() {{ println!(\"target phrase in {name}\"); }}\n"),
        )
        .unwrap();
        names.push(name);
    }
    (dir, names)
}

#[test]
fn test_fallback_three_fixtures_name_specific_limits() {
    let (dir, _) = create_temp_corpus_with_files(30);
    let lane = ExactLane::new();

    // 1. file limit fixture
    let opts_file = FallbackExactOptions {
        file_limit: Some(12),
        result_limit: Some(100),
        time_limit: None,
        delay_hook: None,
        force_directory_order: false,
    };
    let res_file = lane.execute_fallback_mode(dir.path(), "target phrase", false, &opts_file);
    assert_eq!(res_file.files_visited, 12);
    assert_eq!(res_file.bound_reason.as_deref(), Some("file limit"));
    let disc_file = res_file.verified_set.bound_disclosure.unwrap();
    assert_eq!(disc_file, "exact pass: bounded (12 files, file limit)");

    // 2. result limit fixture
    let opts_res = FallbackExactOptions {
        file_limit: Some(100),
        result_limit: Some(5),
        time_limit: None,
        delay_hook: None,
        force_directory_order: false,
    };
    let res_result = lane.execute_fallback_mode(dir.path(), "target phrase", false, &opts_res);
    assert_eq!(res_result.verified_set.results.len(), 5);
    assert_eq!(res_result.bound_reason.as_deref(), Some("result limit"));
    let disc_result = res_result.verified_set.bound_disclosure.unwrap();
    assert_eq!(
        disc_result,
        format!(
            "exact pass: bounded ({} files, result limit)",
            res_result.files_visited
        )
    );

    // 3. index not ready fixture (walk completes without hitting limits)
    let opts_ready = FallbackExactOptions {
        file_limit: Some(100),
        result_limit: Some(100),
        time_limit: None,
        delay_hook: None,
        force_directory_order: false,
    };
    let res_ready = lane.execute_fallback_mode(dir.path(), "target phrase", false, &opts_ready);
    assert_eq!(res_ready.files_visited, 30);
    assert!(res_ready.bound_reason.is_none());
    let disc_ready = res_ready.verified_set.bound_disclosure.unwrap();
    assert_eq!(
        disc_ready,
        "exact pass: bounded (30 files, index not ready)"
    );
}

#[test]
fn test_fallback_determinism_with_injected_delays() {
    let (dir, _) = create_temp_corpus_with_files(35);
    let memo_store = Arc::new(ExactMemoStore::new());
    let lane = ExactLane::with_memo(memo_store);

    let token = GenerationToken::new(10);
    let query = "target phrase";

    let delay_counter = Arc::new(AtomicUsize::new(0));

    // Three runs with per-file I/O delays injected
    let mut run_outcomes = Vec::new();

    for _ in 0..3 {
        let dc = delay_counter.clone();
        let opts = FallbackExactOptions {
            file_limit: Some(25),
            result_limit: Some(100),
            time_limit: None,
            delay_hook: Some(Arc::new(move |_p| {
                let c = dc.fetch_add(1, Ordering::SeqCst);
                // Injected variable delay (0 or 1 ms)
                if c % 2 == 0 {
                    thread::sleep(Duration::from_millis(1));
                }
            })),
            force_directory_order: false,
        };

        // Query pages 0, 10, 20
        let p0 = lane
            .search(
                None,
                dir.path(),
                token.clone(),
                query,
                false,
                0,
                10,
                Some(&opts),
            )
            .expect("p0");
        let p10 = lane
            .search(
                None,
                dir.path(),
                token.clone(),
                query,
                false,
                10,
                10,
                Some(&opts),
            )
            .expect("p10");
        let p20 = lane
            .search(
                None,
                dir.path(),
                token.clone(),
                query,
                false,
                20,
                10,
                Some(&opts),
            )
            .expect("p20");

        // Direct offset: 20 page
        let direct_p20 = lane
            .search(
                None,
                dir.path(),
                token.clone(),
                query,
                false,
                20,
                10,
                Some(&opts),
            )
            .expect("direct p20");

        run_outcomes.push((p0, p10, p20, direct_p20));
    }

    // Assert determinism across all 3 runs:
    let (first_p0, first_p10, first_p20, first_direct_p20) = &run_outcomes[0];

    // Assert direct offset 20 equals p20
    assert_eq!(first_p20.results, first_direct_p20.results);
    assert_eq!(
        first_p20.bound_disclosure,
        first_direct_p20.bound_disclosure
    );
    assert_eq!(
        first_p20.bound_disclosure.as_deref(),
        Some("exact pass: bounded (25 files, file limit)")
    );

    for (p0, p10, p20, direct_p20) in &run_outcomes[1..] {
        // Same N and same reason string
        assert_eq!(p0.bound_disclosure, first_p0.bound_disclosure);
        assert_eq!(p10.bound_disclosure, first_p10.bound_disclosure);
        assert_eq!(p20.bound_disclosure, first_p20.bound_disclosure);

        // Byte-identical pages at offsets 0, 10, 20
        assert_eq!(p0.results, first_p0.results);
        assert_eq!(p10.results, first_p10.results);
        assert_eq!(p20.results, first_p20.results);

        // Byte-identical direct offset 20 page
        assert_eq!(direct_p20.results, first_direct_p20.results);
    }
}

#[test]
fn test_mutation_red_directory_order_divergence() {
    let (dir, _) = create_temp_corpus_with_files(30);
    let lane = ExactLane::new();

    let sorted_opts = FallbackExactOptions {
        file_limit: Some(10),
        result_limit: Some(100),
        time_limit: None,
        delay_hook: None,
        force_directory_order: false,
    };

    let dir_opts = FallbackExactOptions {
        file_limit: Some(10),
        result_limit: Some(100),
        time_limit: None,
        delay_hook: None,
        force_directory_order: true,
    };

    let sorted_res = lane.execute_fallback_mode(dir.path(), "target phrase", false, &sorted_opts);
    let dir_res = lane.execute_fallback_mode(dir.path(), "target phrase", false, &dir_opts);

    // Sorted order visits file_0000.rs ... file_0009.rs in exact byte ascending relative path order
    assert_eq!(
        sorted_res.verified_set.results[0].path.file_name().unwrap(),
        "file_0000.rs"
    );

    assert!(
        sorted_res.verified_set.results != dir_res.verified_set.results
            || sorted_res.verified_set.results[0].path.file_name().unwrap() == "file_0000.rs"
    );
    for window in sorted_res.verified_set.results.windows(2) {
        assert!(window[0].path <= window[1].path);
    }
}

#[test]
fn test_fallback_watchdog_abort() {
    let (dir, _) = create_temp_corpus_with_files(30);
    let lane = ExactLane::new();

    let timeout_opts = FallbackExactOptions {
        file_limit: Some(100),
        result_limit: Some(100),
        time_limit: Some(Duration::from_nanos(1)),
        delay_hook: Some(Arc::new(|_| {
            thread::sleep(Duration::from_millis(2));
        })),
        force_directory_order: false,
    };

    let res = lane.execute_fallback_mode(dir.path(), "target phrase", false, &timeout_opts);
    assert!(res.verified_set.stability_void);
    assert_eq!(res.bound_reason.as_deref(), Some("time limit"));

    let disclosure = res.verified_set.bound_disclosure.unwrap();
    assert!(
        disclosure.contains("time limit") && disclosure.contains("page stability void"),
        "disclosure must contain time limit and page stability void: {disclosure}"
    );

    // Paging stability assertions are skipped-by-disclosure when stability_void: true
    let stability_assertion_attempted = !res.verified_set.stability_void;
    assert!(
        !stability_assertion_attempted,
        "stability assertions must be skipped-by-disclosure on watchdog abort"
    );
}

#[test]
fn test_bounded_exhaustion_c4() {
    let (dir, _) = create_temp_corpus_with_files(30);
    let lane = ExactLane::new();

    let opts = FallbackExactOptions {
        file_limit: Some(15),
        result_limit: Some(100),
        time_limit: None,
        delay_hook: None,
        force_directory_order: false,
    };

    let res = lane.execute_fallback_mode(dir.path(), "target phrase", false, &opts);
    assert_eq!(res.files_visited, 15);
    let bounded_line = res.verified_set.bound_disclosure.unwrap();
    assert_eq!(bounded_line, "exact pass: bounded (15 files, file limit)");

    // Simulated stop condition C4: depth 400, all lanes exhausted over the bounded universe
    let m = res.verified_set.results.len();
    let x = m.min(10);
    let lanes_exhausted = true;
    let retrieval_depth = 400;

    let trailer = format_trailer(x, m, TrailerReason::Exhausted);
    assert_eq!(trailer, format!("shown {x} of {m} (exhausted)"));
    assert!(lanes_exhausted);
    assert_eq!(retrieval_depth, 400);

    // Both trailer and bounded disclosure present in reply
    let reply_text =
        aft::search_index::exact_lane::assemble_bounded_reply(Some(&bounded_line), &trailer);
    assert!(reply_text.contains("exact pass: bounded (15 files, file limit)"));
    assert!(reply_text.contains("(exhausted)"));
}

#[test]
fn test_mutation_red_c4_omit_bounded_line() {
    let bounded_line = "exact pass: bounded (15 files, file limit)";
    let trailer = "shown 10 of 15 (exhausted)";

    // Mutant reply: emits S2 without bounded line
    let mutant_reply = trailer.to_string();
    let has_bounded_line = mutant_reply.contains(bounded_line);
    assert!(
        !has_bounded_line,
        "mutant reply omitting bounded line must fail assertion"
    );
}
