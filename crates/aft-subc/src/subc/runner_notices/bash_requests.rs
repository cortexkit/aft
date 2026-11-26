use crate::protocol::PushFrame;
use crate::subc::bash_notice_text;

pub(in crate::subc) fn notice_send_id(frame: &PushFrame) -> String {
    match frame {
        PushFrame::BashCompleted(frame) => format!("aft:bash:{}", frame.task_id),
        PushFrame::BashLongRunning(frame) => {
            format!("aft:bash-reminder:{}:{}", frame.task_id, frame.elapsed_ms)
        }
        PushFrame::BashPatternMatch(frame) => format!(
            "aft:bash-watch:{}:{}",
            frame.watch_id,
            blake3::hash(&serde_json::to_vec(frame).unwrap())
        ),
        _ => unreachable!(),
    }
}

pub(super) fn parts(frame: &PushFrame) -> (String, String, bool) {
    let id = notice_send_id(frame);
    match frame {
        PushFrame::BashCompleted(_) => (
            id,
            bash_notice_text::completion(&serde_json::to_value(frame).unwrap()),
            true,
        ),
        PushFrame::BashLongRunning(_) => (
            id,
            bash_notice_text::long_running(&serde_json::to_value(frame).unwrap()),
            false,
        ),
        PushFrame::BashPatternMatch(_) => (
            id,
            bash_notice_text::watch(&serde_json::to_value(frame).unwrap()),
            false,
        ),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests;
