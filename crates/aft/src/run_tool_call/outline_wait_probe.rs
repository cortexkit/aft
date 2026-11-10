//! Manual local-file outline measurement. All paths must name a disposable copy.

use super::*;
use crate::data_layer::checkout::{Checkout, DataLayer};
use aft_store::ids::{Digest128, FamilyId};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

fn sample(ctx: &AppContext, root: &std::path::Path, mode: usize, scenario: &str) {
    let args = match mode % 4 {
        0 => serde_json::json!({"target": "crates/aft/src/run_tool_call.rs"}),
        1 => {
            serde_json::json!({"target": ["crates/aft/src/run_tool_call.rs", "crates/aft/src/commands/outline.rs"]})
        }
        2 => serde_json::json!({"target": "crates/aft/src/run_tool_call", "files": true}),
        _ => serde_json::json!({"target": "crates/aft/src/run_tool_call"}),
    };
    let format = crate::subc_format::FormatContext::from_tool_call("outline", &args, root);
    let tool_ctx = ToolCallContext {
        project_root: root.to_path_buf(),
        session_id: Some(format!("outline-probe-{mode}")),
        request_id: format!("outline-probe-{mode}"),
        standard_edit_grammar: true,
        ..ToolCallContext::default()
    };
    let dispatch = |request: RawRequest, ctx: &AppContext| {
        let response = crate::commands::outline::handle_outline(&request, ctx);
        assert!(response.success, "{response:?}");
        response
    };
    let mut trace = PhaseTrace::new(Instant::now());
    trace.mark_executor_submitted();
    trace.mark_job_admitted();
    run_tool_call(
        "outline",
        args,
        &format,
        &tool_ctx,
        ctx,
        &dispatch,
        Some(&|_: &mut Response, _: &mut String| {}),
        Some(&mut trace),
    );
    let now = Instant::now();
    let phases = trace
        .finish(ToolCallEgressTiming {
            enqueued: now,
            dequeued: now,
            write_started: now,
            write_finished: now,
            frame_bytes: 0,
            queue_depth: 0,
            writer_active_at_enqueue: false,
            writer_queue_was_full: false,
            reserve_timeouts: 0,
            prepare_cpu: None,
        })
        .unwrap();
    println!(
        "OUTLINE_PROBE {}",
        serde_json::json!({
            "scenario": scenario, "mode": mode % 4,
            "exec_ms": phases.execute.as_secs_f64() * 1000.0,
            "cpu_ms": phases.cpu.execute.map(|d| d.as_secs_f64() * 1000.0),
            "shape": phases.arg_shape,
        })
    );
}

#[test]
#[ignore = "manual Linux measurement on an isolated repository copy"]
fn outline_off_cpu_wait_probe() {
    let root = std::path::PathBuf::from(
        std::env::var_os("AFT_OUTLINE_WAIT_ROOT").expect("disposable repository copy"),
    )
    .canonicalize()
    .unwrap();
    let scratch = root.parent().unwrap().join("probe-state");
    std::fs::create_dir_all(&scratch).unwrap();
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.clone()),
            storage_dir: Some(scratch.clone()),
            ..crate::config::Config::default()
        },
    ));
    ctx.backup()
        .lock()
        .set_storage_dir(scratch.join("backups"), 72);
    let layer = Arc::new(DataLayer::new(256 << 20, 8));
    let family = FamilyId::from_digest(Digest128::of(b"outline off cpu measurement"));
    let checkout = Arc::new(Checkout::bind(layer, &scratch.join("store"), family, &root).unwrap());
    eprintln!("PROBE build callgraph");
    let started = Instant::now();
    let built = checkout.build().unwrap();
    eprintln!(
        "PROBE built {} files in {:.0}ms",
        built.files,
        started.elapsed().as_secs_f64() * 1000.0
    );
    for mode in 0..8 {
        sample(&ctx, &root, mode, "warmup");
    }
    for mode in 0..40 {
        sample(&ctx, &root, mode, "idle");
    }

    let stop = Arc::new(AtomicBool::new(false));
    std::thread::scope(|scope| {
        let edit_ctx = Arc::clone(&ctx);
        let edit_root = root.clone();
        let edit_stop = Arc::clone(&stop);
        let edit_checkout = Arc::clone(&checkout);
        let writer = scope.spawn(move || {
            let relative = "crates/aft/src/run_tool_call.rs";
            let path = edit_root.join(relative);
            let original = std::fs::read_to_string(&path).unwrap();
            let mut edits = 0;
            while !edit_stop.load(Ordering::Relaxed) {
                let op = format!("probe-edit-{edits}");
                let id = edit_ctx
                    .backup()
                    .lock()
                    .snapshot_with_op("probe-editor", &path, "probe edit", Some(&op))
                    .unwrap();
                assert!(id.is_some(), "backup was skipped");
                crate::edit::write_source_for_probe(
                    &path,
                    format!("// probe {edits}\n{original}").as_bytes(),
                )
                .unwrap();
                edit_ctx.backup().lock().record_post_mutation_states();
                edit_checkout
                    .record_paths(&[relative.as_bytes().to_vec()])
                    .unwrap();
                edits += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            std::fs::write(&path, original).unwrap();
            eprintln!("PROBE edits={edits}");
        });
        let refresh_stop = Arc::clone(&stop);
        let refresh_checkout = Arc::clone(&checkout);
        let refresher = scope.spawn(move || {
            let mut refreshes = 0;
            let mut publications = 0;
            while !refresh_stop.load(Ordering::Relaxed) {
                if refresh_checkout.refresh_callgraph().unwrap().is_some() {
                    refreshes += 1;
                }
                if refresh_checkout.publish_changes().unwrap().is_some() {
                    publications += 1;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            eprintln!("PROBE refreshes={refreshes} publications={publications}");
        });
        let mut readers = Vec::new();
        for mode in 0..4 {
            let ctx = Arc::clone(&ctx);
            let root = root.clone();
            readers.push(scope.spawn(move || {
                let end = Instant::now() + Duration::from_secs(12);
                while Instant::now() < end {
                    sample(&ctx, &root, mode, "edits_refresh");
                    std::thread::sleep(Duration::from_millis(30));
                }
            }));
        }
        for reader in readers {
            reader.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        refresher.join().unwrap();
    });

    // Pausing a real backup owner models a delayed persistence operation without
    // creating CPU load. It isolates whether outline depends on that owner.
    std::thread::scope(|scope| {
        let (arrived_tx, arrived_rx) = std::sync::mpsc::sync_channel(0);
        let ctx_for_writer = Arc::clone(&ctx);
        let path = root.join("crates/aft/src/run_tool_call.rs");
        let owner = scope.spawn(move || {
            let mut store = ctx_for_writer.backup().lock();
            let id = store
                .snapshot_with_op("probe-editor", &path, "delayed probe edit", Some("delayed"))
                .unwrap();
            assert!(id.is_some());
            arrived_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(1200));
        });
        arrived_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut readers = Vec::new();
        for mode in 0..4 {
            let ctx = Arc::clone(&ctx);
            let root = root.clone();
            readers.push(scope.spawn(move || sample(&ctx, &root, mode, "backup_owner_pause")));
        }
        for reader in readers {
            reader.join().unwrap();
        }
        owner.join().unwrap();
    });
}
