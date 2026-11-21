use super::*;
use crate::bash_background::BgTaskStatus;
use crate::protocol::{BashCompletedFrame, BashLongRunningFrame, BashPatternMatchFrame};

#[test]
fn notice_records_keep_runner_notice_text_byte_identical() {
    let finished = PushFrame::BashCompleted(BashCompletedFrame::new(
        "bash-demo",
        "session",
        BgTaskStatus::Completed,
        Some(0),
        "printf hello",
        "hello\n",
        false,
        None,
        None,
        false,
    ));
    let reminder = PushFrame::BashLongRunning(BashLongRunningFrame::new(
        "bash-demo",
        "session",
        "printf hello",
        1_200_000,
    ));
    let watch = PushFrame::BashPatternMatch(BashPatternMatchFrame::new(
        "bash-demo",
        "session",
        "watch",
        "hello",
        0,
        "hello\nother line",
        true,
    ));
    let exit = PushFrame::BashPatternMatch(BashPatternMatchFrame::task_exit(
        "bash-demo",
        "session",
        "exited (exit 0)",
        "task bash-demo exited (exit 0)\nhello",
    ));
    for (frame, expected) in [
        (finished, "<system-reminder>\n[BACKGROUND BASH COMPLETED]\n- task bash-demo (exit 0)\n    hello\n</system-reminder>"),
        (reminder, "<system-reminder>\n[BACKGROUND BASH STILL RUNNING]\n- bash-demo still running after 20m 0s: printf hello\nUse bash_status({ taskId: \"...\" }) to inspect output or bash_kill({ taskId: \"...\" }) to terminate.\n</system-reminder>"),
        (watch, "<system-reminder>\n[BG BASH NOTIFY]\n- task bash-demo matched \"hello\" (offset 0):\n      > hello\n      > other line\n</system-reminder>"),
        (exit, "<system-reminder>\n[BG BASH NOTIFY]\n- task bash-demo exited:\n      > task bash-demo exited (exit 0)\n      > hello\n</system-reminder>"),
    ] {
        let (id, text, completion) = parts(&frame);
        let send = super::super::request(id, text, completion, false);
        let envelope: serde_json::Value = serde_json::from_slice(&super::super::envelope(&send)).unwrap();
        assert_eq!(envelope["params"]["prompt"].as_str().unwrap().as_bytes(), expected.as_bytes());
    }
}
