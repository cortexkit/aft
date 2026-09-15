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

fn observe(breaker: &RepeatBreaker, input: &Value, output: &str, now: Instant) -> Option<String> {
    let intervention = breaker.observe_at(
        SESSION,
        TOOL,
        semantic_key(TOOL, input),
        output_hash(output),
        now,
    )?;
    let mut text = output.to_string();
    append_repeat_breaker_reminder(&mut text, SESSION, &intervention);
    Some(text)
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
fn repeat_breaker_requires_identical_output() {
    let breaker = RepeatBreaker::default();
    let start = Instant::now();
    let input = json!({ "command": "ci status" });

    for (index, output) in ["queued", "running", "complete"].into_iter().enumerate() {
        assert!(observe(
            &breaker,
            &input,
            output,
            start + Duration::from_secs(index as u64 * 31),
        )
        .is_none());
    }
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
fn repeat_breaker_unrelated_tool_resets_the_run() {
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
    assert!(breaker
        .observe_at(
            SESSION,
            "glob",
            semantic_key("glob", &json!({ "pattern": "*.rs" })),
            output_hash(STABLE_OUTPUT),
            start + Duration::from_secs(31),
        )
        .is_none());
    assert!(observe(
        &breaker,
        &bash,
        STABLE_OUTPUT,
        start + Duration::from_secs(62),
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
        if index < 2 {
            std::thread::sleep(Duration::from_secs(16));
        }
    }

    assert_transport_repeat_sequence(&texts);
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
