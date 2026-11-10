use super::wire::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

#[derive(Serialize)]
struct Case<T> {
    tag: &'static str,
    method: &'static str,
    request: serde_json::Value,
    reply: T,
}

fn row(task_id: &str, kind: &str, worker: bool) -> Row {
    Row {
        task_id: task_id.into(),
        revision: 1,
        holder: Holder {
            kind: if worker { "worker" } else { "agent" }.into(),
            agent_id: Some("agent-alf".into()),
            scope_ref: worker.then(|| "scope:worker-cedar".into()),
            execution_id: worker.then(|| "bg_worker_cedar".into()),
            harness: None,
        },
        session: Some(
            if worker {
                "bg_worker_cedar"
            } else {
                "session-alf"
            }
            .into(),
        ),
        call_id: Some(format!("call-{task_id}")),
        title: Some("Run focused tests".into()),
        command: "TOKEN=[REDACTED] cargo test".into(),
        command_truncated: None,
        cwd: "/work/project".into(),
        kind: kind.into(),
        remote: None,
        state: "running".into(),
        started_at_ms: 1_800_000_000_000,
        last_output_at_ms: Some(1_800_000_000_250),
        ended_at_ms: None,
        output_bytes: Offsets {
            stdout: 65_552,
            stderr: 0,
        },
        exit: None,
        end_reason: None,
    }
}

fn ended(mut row: Row) -> Row {
    row.revision += 1;
    row.state = "ended".into();
    row.ended_at_ms = Some(1_800_000_001_000);
    row.exit = Some(Exit {
        code: Some(0),
        signal: None,
    });
    row.end_reason = Some("exited".into());
    row
}

fn status() -> Status {
    let ready = Readiness {
        state: "ready".into(),
        reason: None,
    };
    Status {
        aft: Aft {
            version: "0.64.0".into(),
            card: "ready".into(),
        },
        roots: vec![RootStatus {
            root: "/work/project".into(),
            agent_ids: vec!["agent-alf".into()],
            text_index: ready.clone(),
            call_graph: Readiness {
                state: "building".into(),
                reason: Some("indexing".into()),
            },
            meaning: Readiness {
                state: "disabled".into(),
                reason: Some("not_configured".into()),
            },
            lsp: vec![Lsp {
                server: "rust-analyzer".into(),
                state: ready.state,
                reason: None,
            }],
            errors: 0,
            warnings: 2,
            todos: 3,
        }],
    }
}

fn bytes(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).unwrap();
    bytes.push(b'\n');
    bytes
}

fn vectors() -> Vec<(&'static str, Vec<u8>)> {
    let mut foreground = row("bg_foreground", "foreground", false);
    foreground.output_bytes.stdout = 36;
    let background = row("bg_background", "background", true);
    let pty = row("bg_pty", "pty", true);
    let mut remote = row("bg_remote", "remote", false);
    remote.state = "queued".into();
    remote.remote = Some(Remote {
        job_id: "job-before-acceptance".into(),
        host: "linux".into(),
        phase: "queued".into(),
    });
    let mut truncated = row("bg_long_command", "background", false);
    truncated.command = "x".repeat(2048);
    truncated.command_truncated = Some(true);
    let mut unattributed = row("bg_unattributed", "background", false);
    unattributed.holder = Holder {
        kind: "unattributed".into(),
        agent_id: None,
        scope_ref: None,
        execution_id: None,
        harness: Some("opencode".into()),
    };
    unattributed.session = None;
    unattributed.call_id = None;
    unattributed.title = None;
    let list = ListReply {
        rows: vec![
            foreground.clone(),
            background.clone(),
            pty.clone(),
            remote.clone(),
            truncated,
            unattributed,
        ],
        next_cursor: None,
        unknown_agent_ids: None,
    };
    let mut running = remote.clone();
    running.revision = 2;
    running.state = "running".into();
    running.remote.as_mut().unwrap().phase = "running".into();
    let watch = vec![
        WatchFrame::Resync {
            reason: "initial".into(),
            part: 0,
            final_part: false,
            rows: vec![foreground.clone(), background],
            status: None,
            cursor: None,
        },
        WatchFrame::Resync {
            reason: "initial".into(),
            part: 1,
            final_part: true,
            rows: vec![pty.clone(), remote],
            status: Some(status()),
            cursor: Some("watch-1".into()),
        },
        WatchFrame::Upsert {
            row: row("bg_new", "background", true),
            cursor: "watch-2".into(),
        },
        WatchFrame::Upsert {
            row: running.clone(),
            cursor: "watch-3".into(),
        },
        WatchFrame::Upsert {
            row: ended(running),
            cursor: "watch-4".into(),
        },
        WatchFrame::Remove {
            task_id: "bg_remote".into(),
            cursor: "watch-5".into(),
        },
        WatchFrame::Status {
            status: status(),
            cursor: "watch-6".into(),
        },
        WatchFrame::Resync {
            reason: "lagged".into(),
            part: 0,
            final_part: true,
            rows: vec![foreground.clone()],
            status: Some(status()),
            cursor: Some("watch-7".into()),
        },
    ];
    let keep = vec![
        OutputFrame::Start {
            task_id: foreground.task_id.clone(),
            ansi: "keep".into(),
            truncated_before: Some(Offsets {
                stdout: 16,
                stderr: 0,
            }),
        },
        OutputFrame::Chunk {
            stream: "stdout".into(),
            offset: 16,
            end_offset: 21,
            text: "ab€".into(),
        },
        OutputFrame::Chunk {
            stream: "stdout".into(),
            offset: 21,
            end_offset: 36,
            text: "\u{1b}[32mgreen\u{1b}[0m\n".into(),
        },
        OutputFrame::End {
            row: ended(foreground.clone()),
        },
    ];
    let strip = vec![
        OutputFrame::Start {
            task_id: foreground.task_id.clone(),
            ansi: "strip".into(),
            truncated_before: Some(Offsets {
                stdout: 16,
                stderr: 0,
            }),
        },
        OutputFrame::Chunk {
            stream: "stdout".into(),
            offset: 16,
            end_offset: 21,
            text: "ab€".into(),
        },
        OutputFrame::Chunk {
            stream: "stdout".into(),
            offset: 21,
            end_offset: 36,
            text: "green\n".into(),
        },
        OutputFrame::End {
            row: ended(foreground),
        },
    ];
    let screen = vec![
        OutputFrame::Start {
            task_id: pty.task_id.clone(),
            ansi: "strip".into(),
            truncated_before: None,
        },
        OutputFrame::Screen {
            rows: 2,
            cols: 20,
            lines: vec!["Tests passed".into(), "".into()],
            cursor: ScreenCursor { row: 1, col: 0 },
        },
        OutputFrame::End { row: ended(pty) },
    ];
    let output = vec![
        Case {
            tag: "shells-v1",
            method: "shells.output",
            request: serde_json::json!({"task_id":"bg_foreground", "ansi":"keep", "max_chunk_bytes":16, "from_offset":{"stdout":16,"stderr":0}}),
            reply: keep,
        },
        Case {
            tag: "shells-v1",
            method: "shells.output",
            request: serde_json::json!({"task_id":"bg_foreground", "ansi":"strip", "max_chunk_bytes":16, "from_offset":{"stdout":16,"stderr":0}}),
            reply: strip,
        },
        Case {
            tag: "shells-v1",
            method: "shells.output",
            request: serde_json::json!({"task_id":"bg_pty"}),
            reply: screen,
        },
    ];
    #[derive(Serialize)]
    struct Refusals {
        agent_scoped_route: Error,
        unknown_agent: ListReply,
        shell_not_found: Error,
        invalid_params: Error,
    }
    let refused = Refusals {
        agent_scoped_route: error(
            "shells_operator_required",
            "shells operations require an operator route",
        ),
        unknown_agent: ListReply {
            rows: vec![],
            next_cursor: None,
            unknown_agent_ids: Some(vec!["agent-never-seen".into()]),
        },
        shell_not_found: error("shell_not_found", "shell is not listed"),
        invalid_params: error("invalid_params", "shells.output needs a task_id"),
    };
    vec![
        (
            "list.json",
            bytes(&Case {
                tag: "shells-v1",
                method: "shells.list",
                request: serde_json::json!({}),
                reply: list,
            }),
        ),
        (
            "watch.json",
            bytes(&Case {
                tag: "shells-v1",
                method: "shells.watch",
                request: serde_json::json!({"status":true}),
                reply: watch,
            }),
        ),
        ("output.json", bytes(&output)),
        ("errors.json", bytes(&refused)),
    ]
}

fn directory() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-vectors/shells-v1")
}

fn checksums(vectors: &[(&str, Vec<u8>)]) -> String {
    vectors
        .iter()
        .map(|(name, data)| format!("{:x}  {name}\n", Sha256::digest(data)))
        .collect()
}

#[test]
#[ignore = "regenerates the committed serializer samples"]
fn shells_v1_regenerate_samples() {
    let dir = directory();
    std::fs::create_dir_all(&dir).unwrap();
    let vectors = vectors();
    for (name, data) in &vectors {
        std::fs::write(dir.join(name), data).unwrap();
    }
    std::fs::write(dir.join("SHA256SUMS"), checksums(&vectors)).unwrap();
}

#[test]
fn shells_v1_serializers_match_committed_samples() {
    let dir = directory();
    let vectors = vectors();
    for (name, data) in &vectors {
        assert_eq!(
            &std::fs::read(dir.join(name)).unwrap(),
            data,
            "serializer drift in {name}; regenerate and review the sample"
        );
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("SHA256SUMS")).unwrap(),
        checksums(&vectors)
    );
}

#[test]
fn shells_v1_codes_are_open_and_absent_facts_stay_absent() {
    let mut row = row("future-task", "future-kind", false);
    row.state = "future-state".into();
    row.end_reason = Some("future-end".into());
    row.holder.kind = "future-holder".into();
    let encoded = serde_json::to_value(&row).unwrap();
    assert!(encoded.get("exit").is_none());
    assert!(encoded["holder"].get("scope_ref").is_none());
    assert_eq!(serde_json::from_value::<Row>(encoded).unwrap(), row);
    let readiness = Readiness {
        state: "future-readiness".into(),
        reason: Some("future-reason".into()),
    };
    assert_eq!(
        serde_json::from_slice::<Readiness>(&bytes(&readiness)).unwrap(),
        readiness
    );
}
