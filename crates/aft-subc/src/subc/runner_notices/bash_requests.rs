use crate::protocol::PushFrame;
use crate::subc::bash_notice_text;

pub(super) fn parts(frame: &PushFrame) -> (String, String, bool) {
    match frame {
        PushFrame::BashCompleted(completed) => (
            format!("aft:bash:{}", completed.task_id),
            bash_notice_text::completion(&serde_json::to_value(frame).unwrap()),
            true,
        ),
        PushFrame::BashLongRunning(frame) => (
            format!("aft:bash-reminder:{}:{}", frame.task_id, frame.elapsed_ms),
            bash_notice_text::long_running(&serde_json::to_value(frame).unwrap()),
            false,
        ),
        PushFrame::BashPatternMatch(frame) => (
            format!(
                "aft:bash-watch:{}:{}",
                frame.watch_id,
                blake3::hash(&serde_json::to_vec(frame).unwrap())
            ),
            bash_notice_text::watch(&serde_json::to_value(frame).unwrap()),
            false,
        ),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests;
