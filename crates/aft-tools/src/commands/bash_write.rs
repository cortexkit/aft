use crate::commands::bash_status::format_unknown_write_task_message;
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use serde::Deserialize;
use serde_json::json;

const MAX_INPUT_BYTES: usize = 1_048_576;

/// Input payload accepted by `bash_write`.
///
/// Two forms:
/// * `String` — verbatim bytes written to the PTY. Backward-compatible with the
///   v0.30 phase 1b shape; existing callers see no change.
/// * `Sequence` — array of items, each either a plain string (text bytes) or a
///   `{ "key": "<name>" }` object that expands to a known control-byte sequence
///   (ESC, arrows, Ctrl chords, function keys, …). Items are concatenated into
///   one atomic write so the PTY sees the whole sequence as one input chunk.
///
/// The agent never has to encode escape characters inside the `input` string —
/// they use named keys instead. The string form remains the right choice when
/// the agent wants to write literal `\u001b` characters (e.g. source code).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum BashWriteInput {
    Text(String),
    Sequence(Vec<SequenceItem>),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SequenceItem {
    Text(String),
    Key { key: String },
}

#[derive(Debug, Deserialize)]
pub struct BashWriteParams {
    pub task_id: String,
    pub input: BashWriteInput,
}

#[derive(Debug)]
enum InputError {
    TooLarge,
    InvalidKey(String),
}

impl InputError {
    fn response(self, request_id: &str) -> Response {
        match self {
            Self::TooLarge => Response::error(
                request_id,
                "input_too_large",
                "bash_write input exceeds 1 MiB limit",
            ),
            Self::InvalidKey(message) => Response::error(request_id, "invalid_request", message),
        }
    }
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let raw_params = req.params.get("params").unwrap_or(&req.params);
    // The transport has already parsed JSON, but oversized input must not be
    // cloned into owned parameters before the write budget is checked.
    if let Err(error) = validate_input_size(&raw_params["input"]) {
        return error.response(&req.id);
    }
    let params = match serde_json::from_value::<BashWriteParams>(raw_params.clone()) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash_write: invalid params: {e}"),
            );
        }
    };

    let bytes = match expand_input(&params.input) {
        Ok(bytes) => bytes,
        Err(error) => {
            return error.response(&req.id);
        }
    };

    use crate::bash_background::{PtyWriteError, INPUT_DEADLINE};
    match ctx
        .bash_background()
        .write_pty_input(&params.task_id, req.session(), &bytes)
    {
        Ok(bytes_written) => Response::success(&req.id, json!({ "bytes_written": bytes_written })),
        Err(PtyWriteError::TaskNotFound) => Response::error(
            &req.id,
            "task_not_found",
            format_unknown_write_task_message(&params.task_id, req.host_tool_names().shell()),
        ),
        Err(PtyWriteError::TaskNotPty) => Response::error(
            &req.id,
            "task_not_pty",
            format!("background task is not a PTY task: {}", params.task_id),
        ),
        Err(PtyWriteError::TaskExited) => Response::error(
            &req.id,
            "task_exited",
            format!("PTY task is no longer running: {}", params.task_id),
        ),
        Err(PtyWriteError::InputTimeout { written, pending }) => Response::error_with_data(
            &req.id,
            "input_timeout",
            format!(
                "The program didn't read its terminal input within {}s: wrote {written} of {} bytes; the remaining {pending} bytes were not sent.",
                INPUT_DEADLINE.as_secs(),
                bytes.len()
            ),
            json!({ "bytes_written": written, "bytes_pending": pending }),
        ),
        Err(PtyWriteError::Failed(message)) => Response::error(&req.id, "write_failed", message),
    }
}

fn check_input_size(current: usize, additional: usize) -> Result<(), InputError> {
    if additional > MAX_INPUT_BYTES.saturating_sub(current) {
        Err(InputError::TooLarge)
    } else {
        Ok(())
    }
}

fn validate_input_size(input: &serde_json::Value) -> Result<(), InputError> {
    if let Some(text) = input.as_str() {
        return check_input_size(0, text.len());
    }
    if let Some(items) = input.as_array() {
        let mut total = 0;
        for item in items {
            let size = if let Some(text) = item.as_str() {
                text.len()
            } else if let Some(key) = item.get("key").and_then(serde_json::Value::as_str) {
                // Key normalization can allocate too; bound the borrowed name
                // before mapping it to its shorter terminal byte sequence.
                check_input_size(0, key.len())?;
                key_to_bytes(key).map_or(0, <[u8]>::len)
            } else {
                // Leave malformed shapes to serde's existing diagnostics.
                0
            };
            check_input_size(total, size)?;
            total += size;
        }
    }
    Ok(())
}

fn append_input_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), InputError> {
    check_input_size(out.len(), bytes.len())?;
    out.extend_from_slice(bytes);
    Ok(())
}

fn expand_input(input: &BashWriteInput) -> Result<Vec<u8>, InputError> {
    match input {
        BashWriteInput::Text(s) => {
            check_input_size(0, s.len())?;
            Ok(s.as_bytes().to_vec())
        }
        BashWriteInput::Sequence(items) => {
            let mut out = Vec::new();
            for item in items {
                match item {
                    SequenceItem::Text(s) => append_input_bytes(&mut out, s.as_bytes())?,
                    SequenceItem::Key { key } => {
                        let bytes = key_to_bytes(key).ok_or_else(|| {
                            InputError::InvalidKey(format!(
                                "bash_write: unknown key '{key}'; allowed keys: {}",
                                allowed_keys_hint()
                            ))
                        })?;
                        append_input_bytes(&mut out, bytes)?;
                    }
                }
            }
            Ok(out)
        }
    }
}

/// Map a named key to the byte sequence a terminal sends when that key is pressed.
///
/// Implementation notes:
/// * Names are lowercased and ASCII-only; case-insensitive matching is done by
///   lowercasing the caller-supplied name before lookup.
/// * Control chords `ctrl-a` through `ctrl-z` map programmatically to `0x01..=0x1a`
///   so we don't have to enumerate all 26.
/// * Function keys use the xterm sequence variant (DECFNK / linux-console hybrid)
///   that the vast majority of TUI programs accept.
/// * Arrow / nav keys use the "normal" cursor-key mode sequence (`ESC [ X`)
///   rather than application-keypad mode (`ESC O X`). Programs that toggle
///   application mode (vim with `:set keymodel`) handle both; the normal form
///   is the safer default.
fn key_to_bytes(name: &str) -> Option<&'static [u8]> {
    // Lowercased, hyphen-separated lookup. We allocate only when the input
    // is already non-canonical (rare on the hot path).
    let canonical: std::borrow::Cow<'_, str> = if name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c == '-' || c.is_ascii_digit())
    {
        std::borrow::Cow::Borrowed(name)
    } else {
        std::borrow::Cow::Owned(name.to_ascii_lowercase())
    };

    static TABLE: &[(&str, &[u8])] = &[
        // Line / whitespace
        //
        // ENTER maps to CR (\r, 0x0D) — the byte a real terminal sends when
        // the user presses Enter. Cooked-mode programs (shells, REPLs) have
        // the line discipline translate CR→LF (`icrnl`), so `\r` works for
        // them too. Raw-mode TUIs (opencode TUI, vim insert mode, fzf, htop)
        // see `\r` directly and treat it as submit. LF was wrong for the
        // raw-mode case — opencode TUI would treat `\n` as multi-line input.
        ("enter", b"\r"),
        ("return", b"\r"),
        ("tab", b"\t"),
        ("space", b" "),
        ("backspace", b"\x7f"),
        // Escape
        ("esc", b"\x1b"),
        ("escape", b"\x1b"),
        // Arrows (normal cursor-key mode)
        ("up", b"\x1b[A"),
        ("down", b"\x1b[B"),
        ("right", b"\x1b[C"),
        ("left", b"\x1b[D"),
        // Navigation
        ("home", b"\x1b[H"),
        ("end", b"\x1b[F"),
        ("page-up", b"\x1b[5~"),
        ("page-down", b"\x1b[6~"),
        ("delete", b"\x1b[3~"),
        ("insert", b"\x1b[2~"),
        // Function keys (xterm-style)
        ("f1", b"\x1bOP"),
        ("f2", b"\x1bOQ"),
        ("f3", b"\x1bOR"),
        ("f4", b"\x1bOS"),
        ("f5", b"\x1b[15~"),
        ("f6", b"\x1b[17~"),
        ("f7", b"\x1b[18~"),
        ("f8", b"\x1b[19~"),
        ("f9", b"\x1b[20~"),
        ("f10", b"\x1b[21~"),
        ("f11", b"\x1b[23~"),
        ("f12", b"\x1b[24~"),
    ];

    if let Some((_, bytes)) = TABLE.iter().find(|(n, _)| *n == canonical.as_ref()) {
        return Some(bytes);
    }

    // Ctrl chords: ctrl-a → 0x01 … ctrl-z → 0x1a.
    if let Some(rest) = canonical.strip_prefix("ctrl-") {
        if rest.len() == 1 {
            let c = rest.chars().next().unwrap();
            if c.is_ascii_lowercase() {
                let byte = (c as u8) - b'a' + 1;
                return Some(CTRL_TABLE[byte as usize - 1]);
            }
        }
    }

    None
}

// Pre-materialized byte slices for ctrl-a..ctrl-z so key_to_bytes can return
// `&'static [u8]` without allocating.
static CTRL_TABLE: [&[u8]; 26] = [
    b"\x01", b"\x02", b"\x03", b"\x04", b"\x05", b"\x06", b"\x07", b"\x08", b"\x09", b"\x0a",
    b"\x0b", b"\x0c", b"\x0d", b"\x0e", b"\x0f", b"\x10", b"\x11", b"\x12", b"\x13", b"\x14",
    b"\x15", b"\x16", b"\x17", b"\x18", b"\x19", b"\x1a",
];

fn allowed_keys_hint() -> &'static str {
    "enter, return, tab, space, backspace, esc, escape, up, down, left, right, home, end, \
     page-up, page-down, delete, insert, f1..f12, ctrl-a..ctrl-z"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borrowed_input_budget_is_checked_before_deserializing_params() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(project.path().into()),
                storage_dir: Some(storage.path().into()),
                ..crate::config::Config::default()
            },
        );
        for wrapped in [false, true] {
            let params = json!({ "task_id": false, "input": "x".repeat(MAX_INPUT_BYTES + 1) });
            let request = RawRequest {
                id: "write-preflight".into(),
                command: "bash_write".into(),
                session_id: Some("write-test".into()),
                lsp_hints: None,
                params: if wrapped {
                    json!({ "params": params })
                } else {
                    params
                },
            };
            let reply = handle(&request, &ctx);
            assert_eq!(reply.data["code"], "input_too_large");
            assert_eq!(
                reply.data["message"],
                "bash_write input exceeds 1 MiB limit"
            );
        }
        assert!(matches!(
            validate_input_size(&json!([
                "x".repeat(MAX_INPUT_BYTES - 2), { "key": "up" }
            ])),
            Err(InputError::TooLarge)
        ));
        assert!(validate_input_size(&json!([
            "x".repeat(MAX_INPUT_BYTES - 3), { "key": "up" }
        ]))
        .is_ok());
    }

    #[test]
    fn bash_write_unknown_id_reply_is_one_line_naming_bash() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(project.path().into()),
                storage_dir: Some(storage.path().into()),
                ..crate::config::Config::default()
            },
        );
        for (names, shell) in [(None, "bash"), (Some(json!({ "bash": "shell" })), "shell")] {
            let mut request = json!({
                "id": "write-unknown",
                "command": "bash_write",
                "session_id": "write-test",
                "params": { "task_id": "x", "input": "ls\n" },
            });
            if let Some(names) = names {
                request["host_tool_names"] = names;
            }
            let request: RawRequest = serde_json::from_value(request).unwrap();
            let reply = handle(&request, &ctx);
            assert_eq!(reply.data["code"], "task_not_found", "{:?}", reply.data);
            let message = reply.data["message"].as_str().unwrap();
            assert_eq!(
                message,
                format!(
                    "background task not found: x. To run a command, use `{shell}`; `bash_write` only types into a task that `{shell}` started."
                )
            );
            assert!(!message.contains('\n'), "{message:?}");
        }
    }

    #[test]
    fn rejected_append_does_not_grow_output_buffer() {
        let mut bytes = vec![b'x'; MAX_INPUT_BYTES - 2];
        assert!(matches!(
            append_input_bytes(&mut bytes, b"\x1b[A"),
            Err(InputError::TooLarge)
        ));
        assert_eq!(bytes.len(), MAX_INPUT_BYTES - 2);
        assert!(bytes.iter().all(|byte| *byte == b'x'));
    }

    #[test]
    fn expansion_rejects_oversized_text_and_aggregate_sequence() {
        let text = BashWriteInput::Text("x".repeat(MAX_INPUT_BYTES + 1));
        assert!(expand_input(&text).is_err(), "oversized text was copied");
        let sequence = BashWriteInput::Sequence(vec![
            SequenceItem::Text("x".repeat(MAX_INPUT_BYTES - 2)),
            SequenceItem::Key { key: "up".into() },
        ]);
        assert!(
            expand_input(&sequence).is_err(),
            "oversized sequence was expanded"
        );
    }

    #[test]
    fn expansion_accepts_exact_byte_limit_with_multibyte_text_and_keys() {
        let text = BashWriteInput::Text("é".repeat(MAX_INPUT_BYTES / 2));
        assert_eq!(expand_input(&text).unwrap().len(), MAX_INPUT_BYTES);
        let sequence = BashWriteInput::Sequence(vec![
            SequenceItem::Text("x".repeat(MAX_INPUT_BYTES - 3)),
            SequenceItem::Key { key: "up".into() },
        ]);
        assert_eq!(expand_input(&sequence).unwrap().len(), MAX_INPUT_BYTES);
    }

    #[test]
    fn text_form_passes_bytes_through_verbatim() {
        let input = BashWriteInput::Text("hello\n".into());
        let bytes = expand_input(&input).unwrap();
        assert_eq!(bytes, b"hello\n");
    }

    #[test]
    fn text_form_preserves_literal_escape_sequence_chars() {
        // Critical backward-compat case: the agent wants to write the literal
        // 6 characters \u001b (e.g. into source code), NOT an ESC byte.
        let input = BashWriteInput::Text(r"\u001b[31mred\u001b[0m".into());
        let bytes = expand_input(&input).unwrap();
        assert_eq!(bytes, br"\u001b[31mred\u001b[0m");
        // Sanity: byte count matches the literal char count
        // (6 + 4 + 3 + 6 + 3 = 22), not 8 (the JSON-decoded count) and not
        // some auto-stripped variant.
        assert_eq!(bytes.len(), 22);
    }

    #[test]
    fn sequence_form_expands_text_items() {
        let input = BashWriteInput::Sequence(vec![
            SequenceItem::Text("abc".into()),
            SequenceItem::Text("def".into()),
        ]);
        let bytes = expand_input(&input).unwrap();
        assert_eq!(bytes, b"abcdef");
    }

    #[test]
    fn sequence_form_expands_named_keys_to_byte_sequences() {
        let input = BashWriteInput::Sequence(vec![
            SequenceItem::Key { key: "esc".into() },
            SequenceItem::Key { key: "up".into() },
            SequenceItem::Key {
                key: "ctrl-c".into(),
            },
        ]);
        let bytes = expand_input(&input).unwrap();
        // ESC (\x1b) + arrow-up (\x1b[A) + ctrl-c (\x03)
        assert_eq!(bytes, b"\x1b\x1b[A\x03");
    }

    #[test]
    fn sequence_form_mixes_text_and_keys_in_order() {
        // The vim "type some text, exit insert, save+quit" idiom.
        let input = BashWriteInput::Sequence(vec![
            SequenceItem::Text("iHello".into()),
            SequenceItem::Key { key: "esc".into() },
            SequenceItem::Text(":wq".into()),
            SequenceItem::Key {
                key: "enter".into(),
            },
        ]);
        let bytes = expand_input(&input).unwrap();
        // ENTER maps to CR (\r) for raw-mode TUI compatibility, not LF (\n).
        // See key_to_bytes "Line / whitespace" docs for the rationale.
        assert_eq!(bytes, b"iHello\x1b:wq\r");
    }

    #[test]
    fn sequence_form_accepts_case_insensitive_key_names() {
        let input = BashWriteInput::Sequence(vec![
            SequenceItem::Key { key: "ESC".into() },
            SequenceItem::Key {
                key: "Ctrl-C".into(),
            },
        ]);
        let bytes = expand_input(&input).unwrap();
        assert_eq!(bytes, b"\x1b\x03");
    }

    #[test]
    fn sequence_form_unknown_key_returns_error_with_hint() {
        let input = BashWriteInput::Sequence(vec![SequenceItem::Key {
            key: "windows-key".into(),
        }]);
        let InputError::InvalidKey(err) = expand_input(&input).unwrap_err() else {
            panic!("unknown key must retain its diagnostic");
        };
        assert!(err.contains("unknown key 'windows-key'"));
        assert!(err.contains("allowed keys:"));
    }

    #[test]
    fn ctrl_chord_table_covers_all_26_letters() {
        for (i, letter) in ('a'..='z').enumerate() {
            let name = format!("ctrl-{letter}");
            let bytes = key_to_bytes(&name).unwrap_or_else(|| panic!("missing {name}"));
            assert_eq!(bytes, &[(i as u8) + 1]);
        }
    }

    #[test]
    fn function_keys_use_documented_xterm_sequences() {
        assert_eq!(key_to_bytes("f1"), Some(b"\x1bOP".as_slice()));
        assert_eq!(key_to_bytes("f12"), Some(b"\x1b[24~".as_slice()));
    }

    #[test]
    fn empty_sequence_produces_zero_bytes() {
        let input = BashWriteInput::Sequence(vec![]);
        let bytes = expand_input(&input).unwrap();
        assert_eq!(bytes, b"");
    }

    #[test]
    fn arrows_use_normal_cursor_key_mode_sequence() {
        // ESC [ A/B/C/D form, not ESC O A/B/C/D (application mode).
        assert_eq!(key_to_bytes("up"), Some(b"\x1b[A".as_slice()));
        assert_eq!(key_to_bytes("down"), Some(b"\x1b[B".as_slice()));
        assert_eq!(key_to_bytes("right"), Some(b"\x1b[C".as_slice()));
        assert_eq!(key_to_bytes("left"), Some(b"\x1b[D".as_slice()));
    }
}
