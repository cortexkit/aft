use super::wire::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use subc_protocol::ErrorBody as Error;

#[derive(Serialize)]
struct Case<T> {
    tag: &'static str,
    method: &'static str,
    request: serde_json::Value,
    reply: T,
}

pub(super) fn row(task_id: &str, kind: &str, worker: bool) -> Row {
    let command =
        crate::log_redact::redact_command("TOKEN=fixture-secret-value cargo test").into_owned();
    assert!(!command.contains("fixture-secret-value"));
    assert!(command.contains(crate::log_redact::SECRET_PLACEHOLDER));
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
        command,
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
    if let Some(remote) = &mut row.remote {
        remote.phase = "ended".into();
    }
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
        roots: vec![
            RootStatus {
                agent_ids_truncated: None,
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
                lsp: Some(vec![Lsp {
                    server: "rust-analyzer".into(),
                    state: ready.state.clone(),
                    reason: None,
                }]),
                errors: Some(0),
                warnings: Some(2),
                todos: Some(3),
            },
            RootStatus {
                agent_ids_truncated: None,
                root: "/work/notes".into(),
                agent_ids: vec!["agent-alf".into()],
                text_index: ready.clone(),
                call_graph: ready.clone(),
                meaning: Readiness {
                    state: "disabled".into(),
                    reason: Some("not_configured".into()),
                },
                lsp: Some(vec![]),
                errors: None,
                warnings: None,
                todos: Some(1),
            },
        ],
    }
}

fn bytes(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).unwrap();
    bytes.push(b'\n');
    bytes
}

fn vectors() -> Vec<(&'static str, Vec<u8>)> {
    let edge = format!("{}€", "a".repeat(61));
    let colour = "\u{1b}[32mgreen\u{1b}[0m\n";
    let edge_end = 16 + edge.len() as u64;
    let output_end = edge_end + colour.len() as u64;
    let mut foreground = row("bash-0123456789abcdef", "foreground", false);
    foreground.output_bytes.stdout = output_end;
    let mut background = row("bash-1123456789abcdef", "background", true);
    background.title = Some("Check the API package".into());
    background.command = "cargo check -p api".into();
    background.cwd = "/work/project/api".into();
    let mut pty = row("bash-2123456789abcdef", "pty", true);
    pty.title = Some("Inspect terminal progress".into());
    pty.command = "watch cargo test -p terminal".into();
    pty.cwd = "/work/project/terminal".into();
    let pty_output = "Tests passed\r\n";
    pty.output_bytes.stdout = pty_output.len() as u64;
    let mut remote = row("bash-3123456789abcdef", "remote", false);
    remote.state = "queued".into();
    remote.title = Some("Build the Linux release".into());
    remote.command = "cargo build --release".into();
    remote.cwd = "/work/project/server".into();
    remote.last_output_at_ms = None;
    remote.output_bytes = Offsets::default();
    remote.remote = Some(Remote {
        job_id: "019a667b-1732-7a51-b280-e39d84af0123".into(),
        host: None,
        phase: "queued".into(),
    });
    let mut truncated = row("bash-4123456789abcdef", "background", false);
    truncated.title = Some("Print a generated argument list".into());
    truncated.cwd = "/work/project/tools".into();
    truncated.command = "x".repeat(2048);
    truncated.command_truncated = Some(true);
    let mut unattributed = row("bash-5123456789abcdef", "background", false);
    unattributed.command = "printf 'plain session\\n'".into();
    unattributed.cwd = "/work/notes".into();
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
    let mut placed = row("bash-6123456789abcdef", "remote", true);
    placed.title = Some("Run remote integration tests".into());
    placed.command = "cargo test --test integration".into();
    placed.cwd = "/work/project/tests".into();
    placed.remote = Some(Remote {
        job_id: "019a667b-1733-7b62-8291-e39d84af4567".into(),
        host: Some("ovh-1".into()),
        phase: "running".into(),
    });
    let mut list = ListReply {
        rows: vec![
            foreground.clone(),
            background.clone(),
            pty.clone(),
            remote.clone(),
            placed,
            truncated,
            unattributed,
        ],
        next_cursor: None,
        unknown_agent_ids: None,
    };
    list.rows = super::state::matching_rows(list.rows.iter(), &Filter::default(), None);
    let mut running = remote.clone();
    running.revision = 2;
    running.state = "running".into();
    running.remote.as_mut().unwrap().phase = "running".into();
    running.remote.as_mut().unwrap().host = Some("ovh-1".into());
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
            row: row("bash-7123456789abcdef", "background", true),
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
            task_id: "bash-3123456789abcdef".into(),
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
            end_offset: edge_end,
            text: edge.clone(),
        },
        OutputFrame::Chunk {
            stream: "stdout".into(),
            offset: edge_end,
            end_offset: output_end,
            text: colour.into(),
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
            end_offset: edge_end,
            text: edge,
        },
        OutputFrame::Chunk {
            stream: "stdout".into(),
            offset: edge_end,
            end_offset: output_end,
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
        OutputFrame::Chunk {
            stream: "stdout".into(),
            offset: 0,
            end_offset: pty_output.len() as u64,
            text: pty_output.into(),
        },
        OutputFrame::Screen {
            rows: 2,
            cols: 20,
            lines: crate::pty_render::render_screen(pty_output.as_bytes(), 2, 20)
                .split('\n')
                .map(str::to_owned)
                .collect(),
            cursor: ScreenCursor { row: 1, col: 0 },
        },
        OutputFrame::End { row: ended(pty) },
    ];
    let mut output = vec![
        Case {
            tag: "shells-v1",
            method: "shells.output",
            request: serde_json::json!({"task_id":"bash-0123456789abcdef", "ansi":"keep", "max_chunk_bytes":64, "from_offset":{"stdout":16,"stderr":0}}),
            reply: keep,
        },
        Case {
            tag: "shells-v1",
            method: "shells.output",
            request: serde_json::json!({"task_id":"bash-0123456789abcdef", "ansi":"strip", "max_chunk_bytes":64, "from_offset":{"stdout":16,"stderr":0}}),
            reply: strip,
        },
        Case {
            tag: "shells-v1",
            method: "shells.output",
            request: serde_json::json!({"task_id":"bash-2123456789abcdef"}),
            reply: screen,
        },
    ];
    let mut first_open = row("bash-8123456789abcdef", "background", false);
    first_open.title = Some("Print build progress".into());
    first_open.command = "./build-progress.sh".into();
    first_open.output_bytes = Offsets {
        stdout: BACKFILL_BYTES + 16,
        stderr: 0,
    };
    let mut backfill = vec![OutputFrame::Start {
        task_id: first_open.task_id.clone(),
        ansi: "strip".into(),
        truncated_before: Some(Offsets {
            stdout: 16,
            stderr: 0,
        }),
    }];
    for index in 0..4 {
        let offset = 16 + index * DEFAULT_CHUNK_BYTES as u64;
        backfill.push(OutputFrame::Chunk {
            stream: "stdout".into(),
            offset,
            end_offset: offset + DEFAULT_CHUNK_BYTES as u64,
            text: "o\n".repeat(DEFAULT_CHUNK_BYTES / 2),
        });
    }
    backfill.push(OutputFrame::End {
        row: ended(first_open.clone()),
    });
    output.push(Case {
        tag: "shells-v1",
        method: "shells.output",
        request: serde_json::json!({"task_id":first_open.task_id}),
        reply: backfill,
    });
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
    PathBuf::from(crate::test_support::manifest_dir()).join("../../test-vectors/shells-v1")
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
