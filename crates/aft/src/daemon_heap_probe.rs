//! Isolated engine soak: keep root contexts bound while short sessions come and go.
//! Run alone with `--ignored --nocapture --test-threads=1` for allocator measurements.

use crate::context::{App, AppContext};
use crate::protocol::{RawRequest, Response};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn dispatch(req: RawRequest, ctx: &AppContext) -> Response {
    match req.command.as_str() {
        "grep" => crate::commands::grep::handle_grep(&req, ctx),
        "semantic_search" => crate::commands::semantic_search::handle_semantic_search(&req, ctx),
        "outline" => crate::commands::outline::handle_outline(&req, ctx),
        "edit_match" => crate::commands::edit_match::handle_edit_match(&req, ctx),
        "batch" => crate::commands::batch::handle_batch(&req, ctx),
        other => panic!("unexpected command {other}"),
    }
}

fn call(ctx: &AppContext, session: &str, name: &str, arguments: Value) {
    let req = serde_json::from_value(json!({
        "id":format!("{session}-{name}"), "command":"tool_call", "session_id":session,
        "name":name, "arguments":arguments,
    }))
    .unwrap();
    let response = crate::commands::tool_call::handle_with_dispatch(&req, ctx, &dispatch);
    assert!(response.success, "{response:?}");
}

fn sample(label: &str, roots: &[Arc<AppContext>], start: Instant) {
    let live = crate::test_allocations::live_bytes();
    let allocations = crate::test_allocations::live_allocations();
    let details: Vec<_> = roots.iter().map(|ctx| ctx.memory_root_snapshot()).collect();
    let attributed: u64 = details.iter().map(|row| row.attributed_bytes).sum();
    let memos: Vec<_> = roots
        .iter()
        .map(|ctx| ctx.search_exact_memo().estimated_memory())
        .collect();
    let memo_bytes: u64 = memos.iter().filter_map(|row| row.estimated_bytes).sum();
    let memo_entries: u64 = memos
        .iter()
        .filter_map(|row| row.counts.get("entries"))
        .sum();
    let memo_candidates: u64 = memos
        .iter()
        .filter_map(|row| row.counts.get("candidates"))
        .sum();
    let sqlite = crate::memory::MemorySnapshot::new_uncapped("ready", Default::default())
        .process
        .sqlite
        .memory_used_bytes;
    let io = std::fs::read_to_string("/proc/self/io").unwrap();
    let rchar = io.lines().find(|line| line.starts_with("rchar:")).unwrap();
    eprintln!(
        "heap_soak {label} seconds={:.3} roots={} live_bytes={live} live_allocations={allocations} root_attributed={attributed} memo_bytes={memo_bytes} memo_entries={memo_entries} memo_candidates={memo_candidates} sqlite_bytes={sqlite} {rchar}",
        start.elapsed().as_secs_f64(), roots.len(),
    );
    if let Some(row) = details.first() {
        eprintln!(
            "heap_soak first_root {}",
            serde_json::to_string(row).unwrap()
        );
    }
}

#[test]
#[ignore = "multi-root engine heap and idle-I/O measurement; run in isolation"]
fn synthetic_bound_roots() {
    let _env = crate::test_env::without_storage_override();
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    let app = App::default_shared();
    let root_count: usize = std::env::var("AFT_HEAP_ROOTS")
        .unwrap_or_else(|_| "100".into())
        .parse()
        .unwrap();
    let rounds: usize = std::env::var("AFT_HEAP_ROUNDS")
        .unwrap_or_else(|_| "12".into())
        .parse()
        .unwrap();
    let start = Instant::now();
    let mut roots = Vec::new();
    sample("empty", &roots, start);
    for n in 0..root_count {
        let root = dir.path().join(format!("root-{n}"));
        std::fs::create_dir_all(&root).unwrap();
        for file in 0..16 {
            let mut source = String::from("pub fn target() -> usize { 0 }\n");
            for function in 0..20 {
                source.push_str(&format!("/// Fixture function {function}.\npub fn function_{function}() -> usize {{ target() + {file} }}\n"));
            }
            std::fs::write(root.join(format!("file_{file}.rs")), source).unwrap();
        }
        let mut config = crate::config::Config {
            project_root: Some(root.clone()),
            storage_dir: Some(storage.clone()),
            format_on_edit: false,
            diagnostics_on_edit: false,
            experimental_bash_background: true,
            ..Default::default()
        };
        let ctx = Arc::new(AppContext::from_app(app.clone(), config));
        ctx.mark_subc_bound();
        let req = serde_json::from_value(json!({
            "id":format!("configure-{n}"), "command":"configure", "harness":"runner",
            "project_root":root, "storage_dir":storage, "session_id":format!("initial-{n}"),
            "config":[{"tier":"user", "source":dir.path().join("aft.jsonc"),
                "doc":json!({"indexes":{"trigram":true,"semantic":false,"callgraph":true},
                    "views":{"enabled":true}, "inspect":{"enabled":false},
                    "format_on_edit":false,"diagnostics_on_edit":false,
                    "lsp":{"auto_install":false,"disabled":["rust","typescript","python","go","bash","yaml","ty"]},
                    "disabled_tools":[]}).to_string()}]
        })).unwrap();
        let response = crate::commands::configure::handle_configure(&req, &ctx);
        assert!(response.success, "{response:?}");
        crate::runtime_drain::drain_deferred_configure_maintenance(&ctx);
        call(
            &ctx,
            &format!("initial-{n}"),
            "grep",
            json!({"pattern":"target","path":root}),
        );
        roots.push(ctx);
    }
    sample("bound", &roots, start);
    for round in 0..rounds {
        for (n, ctx) in roots.iter().enumerate() {
            let root = ctx.config().project_root.clone().unwrap();
            let session = format!("session-{round}-{n}");
            ctx.note_configure_session_binding(root.clone(), session.clone());
            call(
                ctx,
                &session,
                "grep",
                json!({"pattern":"function_7","path":root}),
            );
            call(ctx, &session, "search", json!({"query":"target","topK":5}));
            call(
                ctx,
                &session,
                "outline",
                json!({"target":root.join("file_0.rs")}),
            );
            call(
                ctx,
                &session,
                "edit",
                json!({"path":root.join("file_0.rs"),
                "oldString":format!("target() -> usize {{ {round} }}"),
                "newString":format!("target() -> usize {{ {} }}", round + 1)}),
            );
            assert!(std::fs::read_to_string(root.join("file_0.rs"))
                .unwrap()
                .contains(&format!("target() -> usize {{ {} }}", round + 1)));
            if n < 4 {
                let id = ctx
                    .bash_background()
                    .spawn_with_shell_in_slot(
                        crate::sandbox_spawn::SpawnPlan::Unsandboxed,
                        "printf 'heap-probe\\n'",
                        crate::bash_background::BashShell::Bash,
                        "/bin/bash".into(),
                        session.clone(),
                        root.clone(),
                        Default::default(),
                        crate::bash_background::HardKill::After(Duration::from_secs(30)),
                        None,
                        storage.clone(),
                        crate::bash_background::TaskSlot::Background { max: 8 },
                        false,
                        false,
                        Some(root.clone()),
                    )
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    let status = ctx
                        .bash_background()
                        .observed_status(&id, &session, 4096)
                        .unwrap();
                    if status.info.status.is_terminal() {
                        break;
                    }
                    assert!(Instant::now() < deadline, "shell did not complete");
                    std::thread::sleep(Duration::from_millis(10));
                }
                ctx.drain_bg_completions();
            }
            ctx.forget_configure_session_binding(&root, &session);
            ctx.hashline_bindings().teardown(&root, &session);
            ctx.release_backup_session_in_background(session)
                .join()
                .unwrap();
            crate::runtime_drain::drain_watcher_events(ctx);
        }
        std::thread::sleep(Duration::from_secs(1));
        sample(&format!("round-{round}"), &roots, start);
    }
    for second in 1..=3 {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            for ctx in &roots {
                if ctx.watcher_drain_has_work() {
                    crate::runtime_drain::drain_watcher_events(ctx);
                }
                crate::runtime_drain::drain_callgraph_store_events(ctx);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        sample(&format!("idle-{}", second * 10), &roots, start);
    }
    for ctx in &roots {
        ctx.mark_subc_unbound();
        ctx.stop_watcher_runtime();
        ctx.bash_background().shutdown();
        ctx.evict_idle_artifacts();
    }
    sample("quiesced", &roots, start);
    drop(roots);
    sample("dropped", &[], start);
}
