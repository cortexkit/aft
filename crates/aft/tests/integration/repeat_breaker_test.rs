use std::path::Path;
use std::time::{Duration, Instant};

use aft::response_finalize::append_repeat_breaker_reminder;
use aft::response_finalize::repeat_breaker::{output_hash, semantic_key, RepeatBreaker};
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
    let intervention = breaker.observe_at(
        SESSION,
        tool,
        semantic_key(tool, input),
        output_hash(output),
        now,
    )?;
    let mut text = output.to_string();
    append_repeat_breaker_reminder(&mut text, SESSION, &intervention);
    Some(text)
}

fn observe(breaker: &RepeatBreaker, input: &Value, output: &str, now: Instant) -> Option<String> {
    observe_tool(breaker, TOOL, input, output, now)
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
fn repeat_breaker_counts_a_previewed_mutation_once() {
    // Hoisted mutations send a preview and then the apply for one model call.
    // Two genuine identical writes 31 s apart are two occurrences, below the
    // three the breaker needs. Counting previews made them four, and the
    // breaker fired on the second write claiming drifting output.
    let project = tempfile::tempdir().expect("preview repeat project");
    let target = project.path().join("notes.txt");
    let mut aft = AftProcess::spawn();
    aft.configure(project.path());
    let mut texts = Vec::new();

    for cycle in 0..2 {
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
        if cycle == 0 {
            std::thread::sleep(Duration::from_secs(31));
        }
    }

    for (index, text) in texts.iter().enumerate() {
        assert!(
            !text.contains("<system-reminder>"),
            "call {index} must not steer after two genuine writes: {text:?}"
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
