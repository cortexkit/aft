use std::path::Path;
use std::time::{Duration, Instant};

use aft::response_finalize::append_repeat_breaker_reminder;
use aft::response_finalize::repeat_breaker::{output_hash, RepeatBreaker, RepeatCall};
use serde_json::{json, Value};

use crate::test_helpers::AftProcess;

const SESSION: &str = "repeat-breaker-session";
const TOOL: &str = "bash";
const STABLE_OUTPUT: &str = "still waiting";
pub(super) const DESCRIPTIONS: [&str; 3] = ["first poll", "second poll", "third poll"];

pub(super) fn transport_fixture_arguments(root: &Path, description: &str) -> Value {
    json!({
        "pattern": "*.repeat-fixture",
        "path": root,
        "description": description,
    })
}

/// File the rewritten-grep fixtures search; one line matches the needle.
pub(super) const REWRITE_FIXTURE_FILE: &str = "historian.repeat-fixture";

pub(super) fn write_rewritten_grep_fixture(root: &Path) {
    std::fs::write(
        root.join(REWRITE_FIXTURE_FILE),
        "historian response dumped\n",
    )
    .expect("write rewritten grep fixture");
}

/// A bash call that AFT answers with its grep tool instead of running a shell
/// (the bash rewrite), so these fixtures exercise the path where the reply is
/// produced without a spawned process. Only the description varies, as it does
/// when a model repeats a command.
pub(super) fn rewritten_bash_grep_arguments(description: &str) -> Value {
    json!({
        "command": format!("grep -rn \"historian response dumped\" {REWRITE_FIXTURE_FILE}"),
        "description": description,
    })
}

/// The rewritten grep renders a match summary; a native shell grep does not.
pub(super) fn assert_answered_by_rewrite(text: &str) {
    assert!(
        text.contains("Found 1 match"),
        "bash grep must be answered by the rewrite: {text:?}"
    );
}

/// Foreground wait window for the fixtures below: long enough for the quick
/// command to finish inside it, short enough for the slow one to outlive it.
/// A loaded Windows runner has taken longer than 1.5 s just to start the
/// shell for the 0.2 s command, so the window leaves room for that; the slow
/// command sleeps well past it.
pub(super) const REPEAT_FOREGROUND_WAIT_MS: u64 = 4_000;
pub(super) const FINISHED_MARKER: &str = "repeat-native-finished";

/// A native command that finishes inside the foreground wait, so the wait
/// answers it with the command's result.
pub(super) fn finished_bash_arguments(description: &str) -> Value {
    json!({
        "command": format!("sleep 0.2; echo {FINISHED_MARKER}"),
        "description": description,
    })
}

/// A native command that outlives the foreground wait, so the call is answered
/// by promoting the command to a background task.
pub(super) fn promoted_bash_arguments(description: &str) -> Value {
    json!({
        "command": "sleep 10; echo repeat-native-promoted",
        "description": description,
    })
}

/// Rounds 1 and 2 carry no reminder and round 3 names the 3rd call. Counting
/// any call twice would make round 3 the 6th instead.
pub(super) fn assert_third_call_steers(label: &str, texts: &[String], identical: bool) {
    assert_eq!(texts.len(), 3, "{label}: {texts:?}");
    for (index, text) in texts.iter().take(2).enumerate() {
        assert!(
            !text.contains("<system-reminder>"),
            "{label} call {index} must not steer: {text:?}"
        );
    }
    let expected = if identical {
        "This is the 3rd identical call"
    } else {
        "This is the 3rd call with the same arguments"
    };
    assert!(
        texts[2].contains(expected),
        "{label} third call must steer with {expected:?}: {:?}",
        texts[2]
    );
}

pub(super) fn assert_finished_in_foreground(text: &str) {
    assert!(
        text.contains(FINISHED_MARKER) && !text.contains("promoted to background"),
        "quick bash must finish inside the foreground wait: {text:?}"
    );
}

pub(super) fn assert_promoted(text: &str) {
    assert!(
        text.contains("promoted to background"),
        "slow bash must be promoted to the background: {text:?}"
    );
}

pub(super) fn assert_transport_repeat_sequence(texts: &[String]) {
    assert_eq!(texts.len(), 3);
    assert!(
        !texts[0].contains("identical call"),
        "first call: {:?}",
        texts[0]
    );
    assert!(
        !texts[1].contains("identical call"),
        "second call: {:?}",
        texts[1]
    );
    assert!(
        texts[2].contains("This is the 3rd identical call"),
        "third call: {:?}",
        texts[2]
    );
    assert!(texts[2].contains("use a background task with a watch"));
    assert!(texts[2].contains("end the turn"));
}

fn observe_tool(
    breaker: &RepeatBreaker,
    tool: &str,
    input: &Value,
    output: &str,
    now: Instant,
) -> Option<String> {
    observe_tool_as(breaker, tool, input, output, now, false)
}

/// `worker_session` is the caller's role: a delegated worker gets its own
/// wording.
fn observe_tool_as(
    breaker: &RepeatBreaker,
    tool: &str,
    input: &Value,
    output: &str,
    now: Instant,
    worker_session: bool,
) -> Option<String> {
    let intervention = breaker.observe_at(
        SESSION,
        &RepeatCall::new(tool, input),
        output_hash(output),
        now,
    )?;
    let mut text = output.to_string();
    append_repeat_breaker_reminder(
        &mut text,
        SESSION,
        &intervention,
        worker_session,
        worker_session,
    );
    Some(text)
}

fn observe(breaker: &RepeatBreaker, input: &Value, output: &str, now: Instant) -> Option<String> {
    observe_tool(breaker, TOOL, input, output, now)
}

/// What a delegated worker must be told instead of ending its turn: a worker
/// that ends its turn has delivered its result and cannot be woken when the
/// task it waits on finishes.
fn assert_worker_wording(text: &str) {
    assert!(
        text.contains("wait with a watch on its background task"),
        "worker reminder must say to wait on the task: {text:?}"
    );
    assert!(
        !text.contains("end the turn") && !text.contains("turn must end"),
        "worker reminder must not tell it to end its turn: {text:?}"
    );
}

#[test]
fn repeat_breaker_words_its_reminder_for_a_worker() {
    let breaker = RepeatBreaker::default();
    let start = Instant::now();
    let input = json!({ "command": "ci status" });
    let mut texts = Vec::new();
    for index in 0..6 {
        texts.push(observe_tool_as(
            &breaker,
            TOOL,
            &input,
            STABLE_OUTPUT,
            start + Duration::from_secs(index * 16),
            true,
        ));
    }
    let third = texts[2].clone().expect("third identical call must steer");
    assert!(third.contains("This is the 3rd identical call"));
    assert_worker_wording(&third);
    let sixth = texts[5]
        .clone()
        .expect("sixth identical call must escalate");
    assert!(sixth.contains("Stop now: make no further call with these arguments"));
    assert_worker_wording(&sixth);
}

#[test]
fn repeat_breaker_ndjson_carries_the_worker_role_on_tool_call_and_raw_bash() {
    // The plugins attach `worker_session: true` next to `session_id` on every
    // request from a delegated worker: inside the `tool_call` envelope, and at
    // the top level of a raw `bash` request (whose own arguments are nested
    // under `params`).
    let project = tempfile::tempdir().expect("worker repeat project");
    std::fs::write(project.path().join("stable.repeat-fixture"), "stable")
        .expect("write repeat fixture");
    let wait_ms = REPEAT_FOREGROUND_WAIT_MS.to_string();
    let mut aft = AftProcess::spawn_with_env(&[(
        "AFT_TEST_FOREGROUND_WAIT_MS",
        std::ffi::OsStr::new(&wait_ms),
    )]);
    aft.configure(project.path());
    let mut tool_texts = Vec::new();
    let mut bash_texts = Vec::new();

    for (index, description) in DESCRIPTIONS.into_iter().enumerate() {
        let response = aft.send_with_timeout(
            &serde_json::to_string(&json!({
                "id": format!("worker-tool-call-{index}"),
                "command": "tool_call",
                "session_id": SESSION,
                "worker_session": true,
                "name": "glob",
                "arguments": transport_fixture_arguments(project.path(), description),
            }))
            .expect("serialize worker tool call"),
            Duration::from_secs(5),
        );
        tool_texts.push(response["text"].as_str().unwrap_or_default().to_string());

        let mut params = finished_bash_arguments(description);
        let object = params.as_object_mut().expect("bash arguments object");
        object.insert("workdir".to_string(), json!(project.path()));
        object.insert("foreground_orchestrate".to_string(), json!(true));
        let response = aft.send_with_timeout(
            &serde_json::to_string(&json!({
                "id": format!("worker-raw-bash-{index}"),
                "command": "bash",
                // Bash may change files and resets same-session glob repeat
                // counts. Separate sessions let both wording probes reach
                // their third identical call independently.
                "session_id": format!("{SESSION}-bash"),
                "worker_session": true,
                "params": params,
            }))
            .expect("serialize worker raw bash"),
            Duration::from_secs(20),
        );
        bash_texts.push(response["output"].as_str().unwrap_or_default().to_string());
        if index < 2 {
            std::thread::sleep(Duration::from_secs(16));
        }
    }

    assert_third_call_steers("worker tool_call", &tool_texts, true);
    assert_worker_wording(&tool_texts[2]);
    assert_third_call_steers("worker raw bash", &bash_texts, true);
    assert_worker_wording(&bash_texts[2]);
    assert!(aft.shutdown().success());
}

#[test]
fn repeat_breaker_fires_on_third_identical_call_after_thirty_seconds() {
    let breaker = RepeatBreaker::default();
    let start = Instant::now();
    let input = json!({ "command": "ci status" });

    assert!(observe(&breaker, &input, STABLE_OUTPUT, start).is_none());
    assert!(observe(
        &breaker,
        &input,
        STABLE_OUTPUT,
        start + Duration::from_secs(15),
    )
    .is_none());
    let third = observe(
        &breaker,
        &input,
        STABLE_OUTPUT,
        start + Duration::from_secs(42),
    )
    .expect("third identical call spanning at least 30 seconds must steer");

    assert!(third.contains("This is the 3rd identical call"));
    assert!(third.contains("in 42s"));
    assert!(third.contains("use a background task with a watch"));
    assert!(third.contains("end the turn"));
}

#[test]
fn repeat_breaker_uses_semantic_key_ignoring_description() {
    let breaker = RepeatBreaker::default();
    let start = Instant::now();

    for (index, description) in ["first label", "different label", "third label"]
        .into_iter()
        .enumerate()
    {
        let result = observe(
            &breaker,
            &json!({ "command": "ci status", "description": description }),
            STABLE_OUTPUT,
            start + Duration::from_secs(index as u64 * 16),
        );
        if index < 2 {
            assert!(result.is_none());
        } else {
            assert!(result
                .expect("description changes must not reset the semantic run")
                .contains("3rd identical call"));
        }
    }
}

#[test]
fn repeat_breaker_steers_when_output_drifts() {
    let breaker = RepeatBreaker::default();
    let start = Instant::now();
    let input = json!({ "command": "ci status" });
    let mut third = None;

    for (index, output) in ["queued at 10:00", "running at 10:01", "running at 10:02"]
        .into_iter()
        .enumerate()
    {
        third = observe(
            &breaker,
            &input,
            output,
            start + Duration::from_secs(index as u64 * 16),
        );
    }

    let third = third.expect("same arguments must steer even when output changes");
    assert!(third.contains("3rd call with the same arguments"));
    assert!(third.contains("output is drifting"));
    assert!(third.contains("use a background task with a watch"));
}

#[test]
fn repeat_breaker_steers_both_keys_in_interleaved_timestamp_polling() {
    let breaker = RepeatBreaker::default();
    let start = Instant::now();
    let bash_input = json!({
        "command": "cat X | cut -c1-200; date -u +%H:%MZ",
    });
    let status_input = json!({ "taskId": "bash-2026-09-17" });
    let mut third_bash = None;
    let mut third_status = None;

    for round in 0..3 {
        third_bash = observe_tool(
            &breaker,
            "bash",
            &bash_input,
            &format!("task stdout\n10:0{round}Z"),
            start + Duration::from_secs(round * 16),
        );
        third_status = observe_tool(
            &breaker,
            "bash_status",
            &status_input,
            &format!("task still running at 10:0{round}Z"),
            start + Duration::from_secs(round * 16 + 1),
        );
        if round < 2 {
            assert!(third_bash.is_none());
            assert!(third_status.is_none());
        }
    }

    let third_bash = third_bash.expect("third interleaved bash call must steer");
    assert!(third_bash.contains("3rd call with the same arguments"));
    assert!(third_bash.contains("output is drifting"));
    let third_status = third_status.expect("third interleaved bash_status call must steer");
    assert!(third_status.contains("3rd call with the same arguments"));
    assert!(third_status.contains("output is drifting"));
}

#[test]
fn repeat_breaker_steers_third_a_across_three_alternating_keys() {
    let breaker = RepeatBreaker::default();
    let start = Instant::now();
    let inputs = [
        json!({ "command": "A" }),
        json!({ "command": "B" }),
        json!({ "command": "C" }),
    ];
    let sequence = [0, 1, 2, 0, 1, 2, 0];
    let mut seventh = None;

    for (index, input_index) in sequence.into_iter().enumerate() {
        seventh = observe(
            &breaker,
            &inputs[input_index],
            STABLE_OUTPUT,
            start + Duration::from_secs(index as u64 * 6),
        );
        if index < 6 {
            assert!(seventh.is_none());
        }
    }

    let seventh = seventh.expect("third A must steer despite B and C calls between repeats");
    assert!(seventh.contains("This is the 3rd identical call"));
}

#[test]
fn repeat_breaker_never_steers_restores_separated_by_edits() {
    let breaker = RepeatBreaker::default();
    let start = Instant::now();
    let restore = json!({ "op": "restore", "name": "provider-quota-preflight-green" });

    for round in 0..3 {
        assert!(observe_tool(
            &breaker,
            "edit",
            &json!({ "path": "src/quota.rs", "edits": [{ "oldString": "green", "newString": format!("red-{round}") }] }),
            "edited one file",
            start + Duration::from_secs(round * 20),
        )
        .is_none(), "edit round {round} must not steer");
        assert!(
            observe_tool(
                &breaker,
                "aft_safety",
                &restore,
                "restored one file",
                start + Duration::from_secs(round * 20 + 1),
            )
            .is_none(),
            "restore round {round} must not steer"
        );
    }
}

#[test]
fn repeat_breaker_mutation_resets_read_observations() {
    let start = Instant::now();
    let read = json!({ "path": "src/quota.rs" });
    for (mutator, arguments) in mutating_calls().into_iter().chain([
        ("bash", json!({ "command": "touch src/quota.rs" })),
        (
            "powershell",
            json!({ "command": "Set-Content src/quota.rs green" }),
        ),
    ]) {
        let breaker = RepeatBreaker::default();
        for (tool, input, seconds) in [
            ("read", read.clone(), 0),
            (mutator, arguments, 10),
            ("read", read.clone(), 20),
            ("read", read.clone(), 40),
        ] {
            assert!(
                observe_tool(
                    &breaker,
                    tool,
                    &input,
                    STABLE_OUTPUT,
                    start + Duration::from_secs(seconds),
                )
                .is_none(),
                "{mutator}: {tool} at {seconds}s must not steer"
            );
        }
        let third = observe_tool(
            &breaker,
            "read",
            &read,
            STABLE_OUTPUT,
            start + Duration::from_secs(60),
        )
        .expect("three reads after the mutation must still steer");
        assert!(third.contains("This is the 3rd identical call"));
    }
}

fn mutating_calls() -> Vec<(&'static str, Value)> {
    vec![
        (
            "write",
            json!({ "path": "src/quota.rs", "content": "green" }),
        ),
        (
            "edit",
            json!({ "path": "src/quota.rs", "edits": [{ "oldString": "green", "newString": "red" }] }),
        ),
        (
            "apply_patch",
            json!({ "patchText": "*** Begin Patch\n*** Delete File: src/quota.rs\n*** End Patch" }),
        ),
        ("delete", json!({ "files": ["src/quota.rs"] })),
        ("aft_delete", json!({ "files": ["src/quota.rs"] })),
        (
            "move",
            json!({ "path": "src/quota.rs", "destination": "src/quota-old.rs" }),
        ),
        (
            "aft_move",
            json!({ "path": "src/quota.rs", "destination": "src/quota-old.rs" }),
        ),
        (
            "import",
            json!({ "op": "add", "path": "src/quota.rs", "module": "std::fmt" }),
        ),
        (
            "aft_import",
            json!({ "op": "remove", "path": "src/quota.rs", "module": "std::fmt" }),
        ),
        (
            "import",
            json!({ "op": "organize", "path": "src/quota.rs" }),
        ),
        ("safety", json!({ "op": "restore", "name": "green" })),
        (
            "aft_safety",
            json!({ "op": "undo", "path": "src/quota.rs" }),
        ),
        ("aft_safety", json!({ "op": "checkpoint", "name": "green" })),
        (
            "ast_replace",
            json!({ "pattern": "green", "rewrite": "red", "lang": "rust" }),
        ),
        (
            "ast_grep_replace",
            json!({ "pattern": "green", "rewrite": "red", "lang": "rust", "dryRun": false }),
        ),
        ("bash_kill", json!({ "taskId": "task-1" })),
        (
            "bash_write",
            json!({ "taskId": "task-1", "input": "hello" }),
        ),
    ]
}

#[test]
fn repeat_breaker_never_steers_mutating_calls() {
    let start = Instant::now();
    for (tool, input) in mutating_calls() {
        let breaker = RepeatBreaker::default();
        for seconds in [0, 20, 40] {
            assert!(
                observe_tool(
                    &breaker,
                    tool,
                    &input,
                    STABLE_OUTPUT,
                    start + Duration::from_secs(seconds),
                )
                .is_none(),
                "{tool} {input} at {seconds}s must never steer"
            );
        }
    }
}

#[test]
fn repeat_breaker_still_steers_read_only_calls() {
    let start = Instant::now();
    for (tool, input) in [
        ("read", json!({ "path": "src/quota.rs" })),
        ("grep", json!({ "pattern": "green" })),
        ("aft_search", json!({ "query": "green" })),
        ("safety", json!({ "op": "history", "path": "src/quota.rs" })),
        ("aft_safety", json!({ "op": "list" })),
        (
            "ast_replace",
            json!({ "pattern": "green", "rewrite": "red", "lang": "rust", "dryRun": true }),
        ),
        (
            "ast_grep_replace",
            json!({ "pattern": "green", "rewrite": "red", "lang": "rust", "dryRun": true }),
        ),
    ] {
        let breaker = RepeatBreaker::default();
        for seconds in [0, 20] {
            assert!(observe_tool(
                &breaker,
                tool,
                &input,
                STABLE_OUTPUT,
                start + Duration::from_secs(seconds)
            )
            .is_none());
        }
        let third = observe_tool(
            &breaker,
            tool,
            &input,
            STABLE_OUTPUT,
            start + Duration::from_secs(40),
        )
        .unwrap_or_else(|| panic!("{tool} {input}: unchanged read-only calls must steer"));
        assert!(third.contains("This is the 3rd identical call"));
    }
}

#[test]
fn repeat_breaker_mutations_only_reset_the_calling_session() {
    let breaker = RepeatBreaker::default();
    let start = Instant::now();
    let read = RepeatCall::new("read", &json!({ "path": "src/quota.rs" }));
    for seconds in [0, 20] {
        assert!(breaker
            .observe_at(
                SESSION,
                &read,
                output_hash(STABLE_OUTPUT),
                start + Duration::from_secs(seconds)
            )
            .is_none());
    }
    breaker.observe_at(
        "other-session",
        &RepeatCall::new(
            "write",
            &json!({ "path": "src/quota.rs", "content": "red" }),
        ),
        output_hash("written"),
        start + Duration::from_secs(30),
    );
    let third = breaker
        .observe_at(
            SESSION,
            &read,
            output_hash(STABLE_OUTPUT),
            start + Duration::from_secs(40),
        )
        .expect("another session's mutation must not reset this session");
    assert_eq!(third.count, 3);
}

#[test]
fn repeat_breaker_varying_sleep_commands_do_not_hide_task_polling() {
    let breaker = RepeatBreaker::default();
    let start = Instant::now();
    let mut third = None;
    for round in 0..3 {
        assert!(observe_tool(
            &breaker,
            "bash",
            &json!({ "command": format!("sleep {}", round + 1) }),
            "",
            start + Duration::from_secs(round * 20),
        )
        .is_none());
        third = observe_tool(
            &breaker,
            "bash_status",
            &json!({ "taskId": "task-1" }),
            "task still running",
            start + Duration::from_secs(round * 20 + 1),
        );
        if round < 2 {
            assert!(third.is_none());
        }
    }
    assert!(third
        .expect("interleaved sleep commands must not hide status polling")
        .contains("This is the 3rd identical call"));
}

#[test]
fn repeat_breaker_does_not_group_sequential_reads_of_different_files() {
    let breaker = RepeatBreaker::default();
    let start = Instant::now();

    for index in 0..6 {
        assert!(observe_tool(
            &breaker,
            "read",
            &json!({ "path": format!("src/file-{index}.rs") }),
            STABLE_OUTPUT,
            start + Duration::from_secs(index * 10),
        )
        .is_none());
    }
}

#[test]
fn repeat_breaker_groups_execution_knob_changes_under_one_semantic_key() {
    let breaker = RepeatBreaker::default();
    let start = Instant::now();
    let mut third = None;

    for (index, timeout) in [1_000, 2_000, 3_000].into_iter().enumerate() {
        third = observe(
            &breaker,
            &json!({ "command": "ci status", "timeout": timeout }),
            STABLE_OUTPUT,
            start + Duration::from_secs(index as u64 * 16),
        );
        if index < 2 {
            assert!(third.is_none());
        }
    }

    assert!(third
        .expect("execution-knob changes must not split the bash command key")
        .contains("This is the 3rd identical call"));
}

#[test]
fn repeat_breaker_requires_wall_clock_span() {
    let breaker = RepeatBreaker::default();
    let start = Instant::now();
    let input = json!({ "command": "ci status" });

    for seconds in [0, 2, 5] {
        assert!(observe(
            &breaker,
            &input,
            STABLE_OUTPUT,
            start + Duration::from_secs(seconds),
        )
        .is_none());
    }
    let fourth = observe(
        &breaker,
        &input,
        STABLE_OUTPUT,
        start + Duration::from_secs(30),
    )
    .expect("fourth call at 30 seconds must steer");
    assert!(fourth.contains("4th identical call"));
}

#[test]
fn repeat_breaker_tracks_a_key_across_unrelated_calls() {
    let breaker = RepeatBreaker::default();
    let start = Instant::now();
    let bash = json!({ "command": "ci status" });

    assert!(observe(&breaker, &bash, STABLE_OUTPUT, start).is_none());
    assert!(observe(
        &breaker,
        &bash,
        STABLE_OUTPUT,
        start + Duration::from_secs(15),
    )
    .is_none());
    assert!(observe_tool(
        &breaker,
        "glob",
        &json!({ "pattern": "*.rs" }),
        STABLE_OUTPUT,
        start + Duration::from_secs(31),
    )
    .is_none());
    let third = observe(
        &breaker,
        &bash,
        STABLE_OUTPUT,
        start + Duration::from_secs(62),
    )
    .expect("an unrelated tool must not reset the bash key");
    assert!(third.contains("This is the 3rd identical call"));
}

#[test]
fn repeat_breaker_expires_a_key_after_ten_minutes_idle() {
    let input = json!({ "command": "ci status" });
    let start = Instant::now();
    let expired = RepeatBreaker::default();

    assert!(observe(&expired, &input, STABLE_OUTPUT, start).is_none());
    assert!(observe(
        &expired,
        &input,
        STABLE_OUTPUT,
        start + Duration::from_secs(15),
    )
    .is_none());
    assert!(observe(
        &expired,
        &input,
        STABLE_OUTPUT,
        start + Duration::from_secs(15 + 10 * 60 + 1),
    )
    .is_none());

    let active = RepeatBreaker::default();
    assert!(observe(&active, &input, STABLE_OUTPUT, start).is_none());
    assert!(observe(
        &active,
        &input,
        STABLE_OUTPUT,
        start + Duration::from_secs(15),
    )
    .is_none());
    assert!(observe(
        &active,
        &input,
        STABLE_OUTPUT,
        start + Duration::from_secs(15 + 9 * 60),
    )
    .expect("a key idle for less than ten minutes must retain its count")
    .contains("This is the 3rd identical call"));
}

#[test]
fn repeat_breaker_evicts_the_oldest_of_sixty_five_live_keys() {
    let start = Instant::now();
    let oldest = json!({ "command": "oldest" });
    let retained = RepeatBreaker::default();

    assert!(observe(&retained, &oldest, STABLE_OUTPUT, start).is_none());
    assert!(observe(
        &retained,
        &oldest,
        STABLE_OUTPUT,
        start + Duration::from_secs(15),
    )
    .is_none());
    for index in 0..63 {
        assert!(observe(
            &retained,
            &json!({ "command": format!("retained-key-{index}") }),
            STABLE_OUTPUT,
            start + Duration::from_secs(16 + index),
        )
        .is_none());
    }
    assert!(observe(
        &retained,
        &oldest,
        STABLE_OUTPUT,
        start + Duration::from_secs(79),
    )
    .expect("the oldest of sixty-four keys must retain its count")
    .contains("This is the 3rd identical call"));

    let evicted = RepeatBreaker::default();
    assert!(observe(&evicted, &oldest, STABLE_OUTPUT, start).is_none());
    assert!(observe(
        &evicted,
        &oldest,
        STABLE_OUTPUT,
        start + Duration::from_secs(15),
    )
    .is_none());
    for index in 0..64 {
        assert!(observe(
            &evicted,
            &json!({ "command": format!("evicting-key-{index}") }),
            STABLE_OUTPUT,
            start + Duration::from_secs(16 + index),
        )
        .is_none());
    }
    assert!(observe(
        &evicted,
        &oldest,
        STABLE_OUTPUT,
        start + Duration::from_secs(80),
    )
    .is_none());
}

#[test]
fn repeat_breaker_discards_occurrences_beyond_the_record_cap() {
    let start = Instant::now();
    let oldest = json!({ "command": "oldest" });
    let filler = json!({ "command": "filler" });
    let retained = RepeatBreaker::default();

    assert!(observe(&retained, &oldest, STABLE_OUTPUT, start).is_none());
    assert!(observe(
        &retained,
        &oldest,
        STABLE_OUTPUT,
        start + Duration::from_secs(15),
    )
    .is_none());
    for index in 0..253 {
        let _ = observe(
            &retained,
            &filler,
            STABLE_OUTPUT,
            start + Duration::from_secs(16 + index),
        );
    }
    assert!(observe(
        &retained,
        &oldest,
        STABLE_OUTPUT,
        start + Duration::from_secs(269),
    )
    .expect("all three oldest-key calls fit in a 256-record ring")
    .contains("This is the 3rd identical call"));

    let evicted = RepeatBreaker::default();
    assert!(observe(&evicted, &oldest, STABLE_OUTPUT, start).is_none());
    assert!(observe(
        &evicted,
        &oldest,
        STABLE_OUTPUT,
        start + Duration::from_secs(15),
    )
    .is_none());
    for index in 0..254 {
        let _ = observe(
            &evicted,
            &filler,
            STABLE_OUTPUT,
            start + Duration::from_secs(16 + index),
        );
    }
    assert!(observe(
        &evicted,
        &oldest,
        STABLE_OUTPUT,
        start + Duration::from_secs(270),
    )
    .is_none());
}

#[test]
fn repeat_breaker_ndjson_real_binary_uses_shared_transport_fixture() {
    let project = tempfile::tempdir().expect("repeat breaker NDJSON project");
    std::fs::write(project.path().join("stable.repeat-fixture"), "stable")
        .expect("write repeat fixture");
    let mut aft = AftProcess::spawn();
    aft.configure(project.path());
    let mut texts = Vec::new();

    for (index, description) in DESCRIPTIONS.into_iter().enumerate() {
        let response = aft.send_with_timeout(
            &serde_json::to_string(&json!({
                "id": format!("repeat-ndjson-{index}"),
                "command": "tool_call",
                "session_id": SESSION,
                "name": "glob",
                "arguments": transport_fixture_arguments(project.path(), description),
            }))
            .expect("serialize repeat request"),
            Duration::from_secs(5),
        );
        let text = response["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tool response missing text: {response:?}"))
            .to_string();
        assert!(!text.is_empty(), "empty tool response text: {response:?}");
        texts.push(text);
        // The plugin drains background completions after every agent tool call
        // under the same session. The drain must neither reset the agent call's
        // key nor accumulate its own repeat count.
        let drain = aft.send_with_timeout(
            &serde_json::to_string(&json!({
                "id": format!("repeat-ndjson-drain-{index}"),
                "command": "tool_call",
                "session_id": SESSION,
                "name": "bash_drain_completions",
                "arguments": { "session_id": SESSION },
            }))
            .expect("serialize drain request"),
            Duration::from_secs(5),
        );
        assert!(
            drain["success"].as_bool().unwrap_or(false),
            "drain between agent calls must succeed: {drain:?}"
        );
        assert!(
            !drain["text"]
                .as_str()
                .unwrap_or_default()
                .contains("<system-reminder>"),
            "plumbing calls must not accumulate a repeat count: {drain:?}"
        );
        if index < 2 {
            std::thread::sleep(Duration::from_secs(16));
        }
    }

    assert_transport_repeat_sequence(&texts);
    assert!(aft.shutdown().success());
}

#[test]
fn repeat_breaker_ndjson_tool_call_steers_a_rewritten_bash_grep() {
    let project = tempfile::tempdir().expect("rewritten bash repeat project");
    write_rewritten_grep_fixture(project.path());
    let mut aft = AftProcess::spawn();
    let configured = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.path(),
            "config": crate::test_helpers::user_config(json!({
                "bash": { "rewrite": true },
                "indexes": { "trigram": false, "semantic": false, "callgraph": false },
            })),
        })
        .to_string(),
    );
    assert_eq!(configured["success"], true, "configure: {configured:?}");
    let mut texts = Vec::new();

    for (index, description) in DESCRIPTIONS.into_iter().enumerate() {
        let response = aft.send_with_timeout(
            &serde_json::to_string(&json!({
                "id": format!("repeat-ndjson-bash-{index}"),
                "command": "tool_call",
                "session_id": SESSION,
                "name": "bash",
                "arguments": rewritten_bash_grep_arguments(description),
            }))
            .expect("serialize rewritten bash request"),
            Duration::from_secs(10),
        );
        let text = response["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tool response missing text: {response:?}"))
            .to_string();
        assert_answered_by_rewrite(&text);
        texts.push(text);
        if index < 2 {
            std::thread::sleep(Duration::from_secs(16));
        }
    }

    assert_transport_repeat_sequence(&texts);
    assert!(aft.shutdown().success());
}

#[test]
fn repeat_breaker_standalone_raw_bash_steers_on_every_answer_path() {
    // The Pi plugin and the OpenCode plugin in standalone mode send each model
    // bash call as a top-level `bash` request, arguments nested under `params`,
    // and show the model the response's `output`.
    let project = tempfile::tempdir().expect("raw bash repeat project");
    write_rewritten_grep_fixture(project.path());
    let wait_ms = REPEAT_FOREGROUND_WAIT_MS.to_string();
    let mut aft = AftProcess::spawn_with_env(&[(
        "AFT_TEST_FOREGROUND_WAIT_MS",
        std::ffi::OsStr::new(&wait_ms),
    )]);
    let configured = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "pi",
            "project_root": project.path(),
            "config": crate::test_helpers::user_config(json!({
                "bash": { "rewrite": true },
                "indexes": { "trigram": false, "semantic": false, "callgraph": false },
            })),
        })
        .to_string(),
    );
    assert_eq!(configured["success"], true, "configure: {configured:?}");
    let mut rewritten = Vec::new();
    let mut finished = Vec::new();
    let mut promoted = Vec::new();

    for (index, description) in DESCRIPTIONS.into_iter().enumerate() {
        for (kind, arguments) in [
            ("rewritten", rewritten_bash_grep_arguments(description)),
            ("finished", finished_bash_arguments(description)),
            ("promoted", promoted_bash_arguments(description)),
        ] {
            let mut params = arguments;
            let object = params.as_object_mut().expect("bash arguments object");
            object.insert("workdir".to_string(), json!(project.path()));
            object.insert("background".to_string(), json!(false));
            object.insert("notify_on_completion".to_string(), json!(false));
            object.insert("pty".to_string(), json!(false));
            object.insert("foreground_orchestrate".to_string(), json!(true));
            object.insert("block_to_completion".to_string(), json!(false));
            object.insert("wait".to_string(), json!(false));
            let response = aft.send_with_timeout(
                &serde_json::to_string(&json!({
                    "id": format!("repeat-raw-bash-{kind}-{index}"),
                    "command": "bash",
                    "session_id": SESSION,
                    "params": params,
                }))
                .expect("serialize raw bash request"),
                Duration::from_secs(20),
            );
            assert_eq!(response["success"], true, "{kind} raw bash: {response:?}");
            let output = response["output"]
                .as_str()
                .unwrap_or_else(|| panic!("raw bash response missing output: {response:?}"))
                .to_string();
            match kind {
                "rewritten" => {
                    assert_answered_by_rewrite(&output);
                    rewritten.push(output);
                }
                "finished" => {
                    assert_finished_in_foreground(&output);
                    finished.push(output);
                }
                _ => {
                    assert_promoted(&output);
                    promoted.push(output);
                }
            }
        }
        if index < 2 {
            std::thread::sleep(Duration::from_secs(16));
        }
    }

    assert_third_call_steers("rewritten grep", &rewritten, true);
    assert_third_call_steers("foreground-finished bash", &finished, true);
    // Each promotion names a new task, so the output drifts.
    assert_third_call_steers("promoted bash", &promoted, false);
    assert!(aft.shutdown().success());
}

#[test]
fn repeat_breaker_never_steers_previewed_mutations() {
    // Hoisted mutations send a preview and then the apply for one model call.
    // Neither half is polling, even after three identical applied writes span
    // the breaker's time threshold.
    let project = tempfile::tempdir().expect("preview repeat project");
    let target = project.path().join("notes.txt");
    let mut aft = AftProcess::spawn();
    aft.configure(project.path());
    let mut texts = Vec::new();

    for cycle in 0..3 {
        for preview in [true, false] {
            let mut request = json!({
                "id": format!("preview-repeat-{cycle}-{preview}"),
                "command": "tool_call",
                "session_id": SESSION,
                "name": "write",
                "arguments": { "path": target, "content": "same body\n" },
            });
            if preview {
                request["preview"] = json!(true);
            }
            let response = aft.send_with_timeout(
                &serde_json::to_string(&request).expect("serialize write request"),
                Duration::from_secs(10),
            );
            assert!(
                response["success"].as_bool().unwrap_or(false),
                "write must succeed: {response:?}"
            );
            texts.push(response["text"].as_str().unwrap_or_default().to_string());
        }
        if cycle < 2 {
            // Remove the file so the next identical write creates it again;
            // writing unchanged content is refused as `no_change`.
            std::fs::remove_file(&target).expect("remove written file");
            std::thread::sleep(Duration::from_secs(31));
        }
    }

    for (index, text) in texts.iter().enumerate() {
        assert!(
            !text.contains("<system-reminder>"),
            "call {index} must not steer after three genuine writes: {text:?}"
        );
    }
    assert!(aft.shutdown().success());
}

#[test]
fn repeat_breaker_escalates_from_sixth_call() {
    let breaker = RepeatBreaker::default();
    let start = Instant::now();
    let input = json!({ "command": "ci status" });
    let mut sixth = None;

    for index in 0..6 {
        sixth = observe(
            &breaker,
            &input,
            STABLE_OUTPUT,
            start + Duration::from_secs(index * 10),
        );
    }

    let sixth = sixth.expect("sixth identical call must escalate");
    assert!(sixth.contains("This is the 6th identical call"));
    assert!(sixth.contains("The turn must end now with no further tool call"));
}
