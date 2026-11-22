use crate::bash_background::{persistence::BgMode, BgTaskStatus};
use serde_json::{json, Value};

// The tool-display text_lines field caps both the array and its line text.
const MAX_LINES: usize = 3;
const MAX_CHARS: usize = 200;

pub(crate) fn from_streams(
    metadata: &crate::bash_background::persistence::PersistedTask,
    stdout: &str,
    stderr: &str,
    remask: impl Fn(&str) -> Option<String>,
) -> Option<Value> {
    let stdout = clean_stream(stdout, &remask)?;
    let stderr = if metadata.mode == BgMode::Pty {
        // PTYs capture one combined stream; it is already in stdout.
        std::borrow::Cow::Borrowed("")
    } else {
        clean_stream(stderr, &remask)?
    };
    let mut tail: Vec<_> = stderr
        .lines()
        .rev()
        .map(|line| (line, true))
        .chain(stdout.lines().rev().map(|line| (line, false)))
        .filter(|(line, _)| !line.is_empty())
        .take(MAX_LINES)
        .enumerate()
        .map(|(index, (line, stderr))| {
            let end = line
                .char_indices()
                .nth(MAX_CHARS)
                .map_or(line.len(), |(i, _)| i);
            let mut item = json!({"text": &line[..end]});
            if index == 0
                && (metadata.exit_code.is_some_and(|code| code != 0)
                    || matches!(
                        metadata.status,
                        BgTaskStatus::Killed | BgTaskStatus::TimedOut
                    ))
            {
                item["tone"] = json!("danger");
            } else if stderr {
                item["tone"] = json!("muted");
            }
            item
        })
        .collect();
    tail.reverse();
    (!tail.is_empty()).then_some(Value::Array(tail))
}

fn clean_stream<'a>(
    stream: &'a str,
    remask: &impl Fn(&str) -> Option<String>,
) -> Option<std::borrow::Cow<'a, str>> {
    if !stream.chars().any(|ch| ch != '\n' && ch.is_control()) {
        return Some(std::borrow::Cow::Borrowed(stream));
    }

    #[derive(Default)]
    struct PlainText(String);
    impl vte::Perform for PlainText {
        fn print(&mut self, ch: char) {
            if !ch.is_control() {
                self.0.push(ch);
            }
        }

        fn execute(&mut self, byte: u8) {
            if byte == b'\n' {
                self.0.push('\n');
            }
        }
    }

    let mut plain = PlainText::default();
    vte::Parser::new().advance(&mut plain, stream.as_bytes());
    // Removing an escape or control character can rejoin fragments of a
    // secret. Mask the intact cleaned stream again before cutting any line.
    remask(&plain.0).map(std::borrow::Cow::Owned)
}
