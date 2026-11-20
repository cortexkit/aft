use super::*;
use crate::bash_background::BgTaskStatus;
use crate::protocol::{BashCompletedFrame, BashLongRunningFrame, BashPatternMatchFrame};
use std::ffi::OsStr;

fn metadata() -> PersistedTask {
    let mut task = PersistedTask::starting(
        "bash-demo".into(),
        "session".into(),
        "printf hello".into(),
        ".".into(),
        None,
        None,
        true,
        false,
    );
    task.status = BgTaskStatus::Completed;
    task.duration_ms = Some(22 * 60 * 1000);
    task.exit_code = Some(0);
    task
}

fn frames() -> [PushFrame; 4] {
    [
        PushFrame::BashCompleted(BashCompletedFrame::new(
            "bash-demo",
            "session",
            BgTaskStatus::Completed,
            Some(0),
            "printf hello",
            "hello",
            false,
            None,
            None,
            false,
        )),
        PushFrame::BashLongRunning(BashLongRunningFrame::new(
            "bash-demo",
            "session",
            "printf hello",
            1_200_000,
        )),
        PushFrame::BashPatternMatch(BashPatternMatchFrame::new(
            "bash-demo",
            "session",
            "watch",
            "hello",
            0,
            "hello\nother line",
            true,
        )),
        PushFrame::BashPatternMatch(BashPatternMatchFrame::task_exit(
            "bash-demo",
            "session",
            "exited (exit 0)",
            "task bash-demo exited (exit 0)\nhello",
        )),
    ]
}

#[test]
fn notice_records_cover_each_send_and_fold_only_task_reminders_and_completions() {
    let metadata = metadata();
    let records: Vec<_> = frames()
        .iter()
        .enumerate()
        .map(|(i, frame)| {
            let send_id = format!("send:{i}");
            let body = body(frame, &metadata, metadata.started_at, str::to_owned).unwrap();
            let record = pull("broca", "session", &send_id, body.clone()).unwrap();
            let write: NoticeWrite = serde_json::from_slice(&record.bytes).unwrap();
            assert_eq!(write.send_id, send_id);
            assert_eq!(write.session, "session");
            assert_eq!(write.kind, "notice");
            assert_eq!(write.body, body);
            assert_ne!(
                record.id,
                pull("broca", "other-session", &send_id, body.clone())
                    .unwrap()
                    .id
            );
            assert_eq!(
                record.id,
                pull("other-runner", "session", &send_id, body).unwrap().id
            );
            (record.id, write.body)
        })
        .collect();
    assert_eq!(
        records
            .iter()
            .map(|(id, _)| id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4
    );
    let [(_, finished), (_, running), (_, watch), (_, exit)] = records.as_slice() else {
        unreachable!()
    };
    assert_eq!(finished.view, "bash_finished:1");
    assert_eq!(running.view, "bash_still_running:1");
    assert_eq!(watch.view, "bash_watch:1");
    assert_eq!(exit.view, watch.view);
    assert_eq!(finished.key.as_deref(), Some("aft:bash-task:bash-demo"));
    assert_eq!(finished.key, running.key);
    assert!(watch.key.is_none() && exit.key.is_none());
    assert_eq!(finished.data["duration_ms"], 1_320_000);
    assert_eq!(finished.data["exit_code"], 0);
    assert_eq!(running.data["elapsed_ms"], 1_200_000);
    assert_eq!(running.data["status"], "running");
    assert!(!running.data.contains_key("exit_code"));
    assert_eq!(watch.data["watch_reason"], "pattern_match");
    assert_eq!(watch.data["watch_match"], "hello");
    assert_eq!(exit.data["watch_reason"], "task_exit");
    assert_eq!(exit.data["output_preview"], "hello");
}

#[test]
fn notice_records_cut_tail_on_utf8_boundaries_and_budget_for_json_escapes() {
    let mut data = super::super::records::object(json!({"command":"kept"}));
    data.insert(
        "padding".into(),
        json!("x".repeat(MAX_NOTICE_RECORD_BYTES - 120)),
    );
    let text = format!("{}終", "\u{0001}".repeat(301));
    let original = NoticeBody::new("bash_finished:1", None, data);
    let mut full_preview = original.clone();
    full_preview
        .data
        .insert("output_preview".into(), json!(bounded_tail(&text, 300)));
    full_preview.data.insert("output_cut".into(), json!(true));
    assert!(
        full_preview.check().is_err(),
        "the 300-byte tail alone exceeds the canonical limit"
    );
    let cut = fit_preview(original, &text, false).unwrap();
    assert!(cut.check().unwrap().len() <= MAX_NOTICE_RECORD_BYTES);
    assert_eq!(cut.data["output_cut"], true);
    assert_eq!(cut.data["command"], "kept");
    let tail = cut.data["output_preview"].as_str().unwrap();
    assert!(tail.ends_with('終') && text.ends_with(tail) && tail.len() < 300);
}

#[test]
fn notice_records_apply_mask_to_every_text_field_before_cutting() {
    let mut metadata = metadata();
    metadata.command = "printf private-value".into();
    let mut value = serde_json::to_value(&metadata).unwrap();
    value["shell_call"] =
        json!({"description":"Show private-value", "session":"session", "kind":"bash"});
    metadata = serde_json::from_value(value).unwrap();
    let frame = PushFrame::BashPatternMatch(BashPatternMatchFrame::new(
        "bash-demo",
        "session",
        "watch",
        "private-value",
        0,
        format!("{}private-value", "é".repeat(301)),
        true,
    ));
    let secrets = crate::log_redact::SecretValues::from_environment([(
        OsStr::new("TEST_TOKEN"),
        OsStr::new("private-value"),
    )]);
    let body = body(&frame, &metadata, metadata.started_at, |text| {
        secrets.redact(text).into_owned()
    })
    .unwrap();
    assert!(!String::from_utf8(body.check().unwrap())
        .unwrap()
        .contains("private-value"));
    let placeholder = crate::log_redact::SECRET_PLACEHOLDER;
    assert_eq!(body.data["description"], format!("Show {placeholder}"));
    assert_eq!(body.data["watch_match"], placeholder);
    assert!(body.data["command"].as_str().unwrap().contains(placeholder));
    assert!(body.data["output_preview"]
        .as_str()
        .unwrap()
        .ends_with(placeholder));
    assert_eq!(body.data["output_cut"], true);
}

#[test]
fn notice_records_default_switch_skips_building_a_pull_record() {
    // The same check in the enabled build is covered by reader discovery tests.
    if WRITER_CAPABILITY.is_none() {
        let registry = crate::bash_background::BgTaskRegistry::new(Default::default());
        for frame in frames() {
            assert!(for_send(
                &registry,
                &frame,
                &metadata(),
                "broca",
                "session",
                "send",
                0
            )
            .is_none());
        }
    }
}
