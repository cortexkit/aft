//! Best-effort foreground previews. File reads and redaction never run on a request thread.

use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

use super::persistence::TaskArtifact;
use super::BgTaskRegistry;
use crate::context::AppContext;
use crate::protocol::{ProgressFrame, ProgressKind};

const INTERVAL: Duration = Duration::from_millis(250);
const FRAME_BYTES: usize = 4 * 1024;
const CALL_BYTES: usize = 1024 * 1024;

#[derive(Default)]
struct StreamCursor {
    offset: u64,
    position: u64,
    line_start: u64,
    pending: Vec<u8>,
    skipping_line: bool,
    private_key: bool,
}

impl StreamCursor {
    fn accept(
        &mut self,
        bytes: &[u8],
        reset: bool,
        mask: &impl Fn(u64, &mut String),
    ) -> Vec<String> {
        if reset {
            self.pending.clear();
            // A replaced artifact can begin in the middle of a credential.
            self.skipping_line = true;
        }
        let mut lines = Vec::new();
        let mut chunk = String::new();
        let mut chunk_start = 0;
        for &byte in bytes {
            self.position += 1;
            if self.skipping_line {
                if byte == b'\n' {
                    self.skipping_line = false;
                    self.line_start = self.position;
                }
                continue;
            }
            self.pending.push(byte);
            if self.pending.len() > FRAME_BYTES {
                // A line too large for a frame stays exclusively in the final
                // reply. Never split an unredacted credential across frames.
                self.pending.clear();
                self.skipping_line = byte != b'\n';
                if byte == b'\n' {
                    self.line_start = self.position;
                }
                continue;
            }
            if byte != b'\n' {
                continue;
            }
            let raw = std::mem::take(&mut self.pending);
            let from = self.line_start;
            self.line_start = self.position;
            let Ok(text) = String::from_utf8(raw) else {
                continue;
            };
            if text.contains("-----BEGIN ") && text.contains("PRIVATE KEY-----") {
                self.private_key = true;
            }
            if self.private_key {
                if text.contains("-----END ") && text.contains("PRIVATE KEY-----") {
                    self.private_key = false;
                    mask_chunk(&mut chunk, chunk_start, mask, &mut lines);
                    lines.push("[REDACTED]\n".to_string());
                }
                continue;
            }
            if chunk.len() + text.len() > FRAME_BYTES || from != chunk_start + chunk.len() as u64 {
                mask_chunk(&mut chunk, chunk_start, mask, &mut lines);
            }
            if chunk.is_empty() {
                chunk_start = from;
            }
            chunk.push_str(&text);
        }
        mask_chunk(&mut chunk, chunk_start, mask, &mut lines);
        lines
    }

    fn tail(&self, mask: &impl Fn(u64, &mut String)) -> Option<String> {
        if self.private_key || self.skipping_line || self.pending.is_empty() {
            return None;
        }
        let mut text = std::str::from_utf8(&self.pending).ok()?.to_string();
        // Incomplete key blocks are never safe to flush at the end of a call.
        if text.contains("-----BEGIN ") {
            return None;
        }
        mask(self.line_start, &mut text);
        (text.len() <= FRAME_BYTES).then_some(text)
    }
}

fn mask_chunk(
    chunk: &mut String,
    from: u64,
    mask: &impl Fn(u64, &mut String),
    output: &mut Vec<String>,
) {
    if chunk.is_empty() {
        return;
    }
    let mut text = std::mem::take(chunk);
    mask(from, &mut text);
    if text.len() <= FRAME_BYTES {
        output.push(text);
    }
}

#[derive(Default)]
struct Batch {
    lines: Vec<(&'static str, String)>,
    tails: Vec<(&'static str, String)>,
}

/// Owns only bounded channels. Dropping a call never joins a blocked file reader.
pub(crate) struct BashProgressPump {
    tick: Option<SyncSender<()>>,
    ready: Option<Receiver<Batch>>,
    request_id: String,
    next_tick: Instant,
    next_emit: Instant,
    tails: Vec<(&'static str, String)>,
    emitted: usize,
}

impl BashProgressPump {
    pub(crate) fn new(
        ctx: &AppContext,
        task_id: String,
        session_id: String,
        request_id: String,
    ) -> Self {
        let registry = ctx.bash_background().clone();
        let (tick, ticks) = mpsc::sync_channel(1);
        let (results, ready) = mpsc::sync_channel(1);
        // There is at most one outstanding read. Slow artifacts cannot create
        // an unbounded queue or delay a terminal response.
        let _ = std::thread::Builder::new()
            .name("bash-progress".into())
            .spawn(move || {
                let mut streams = [StreamCursor::default(), StreamCursor::default()];
                while ticks.recv().is_ok() {
                    let batch = read_batch(&registry, &task_id, &session_id, &mut streams);
                    if results.send(batch).is_err() {
                        break;
                    }
                }
            });
        Self {
            tick: Some(tick),
            ready: Some(ready),
            request_id,
            next_tick: Instant::now(),
            next_emit: Instant::now(),
            tails: Vec::new(),
            emitted: 0,
        }
    }

    pub(crate) fn poll(&mut self, terminal: bool, emit: &mut impl FnMut(ProgressFrame)) {
        if self.tick.is_none() {
            return;
        }
        if terminal || Instant::now() >= self.next_emit {
            if let Some(batch) = self.ready.as_ref().and_then(|ready| ready.try_recv().ok()) {
                self.tails = batch.tails;
                self.emit(batch.lines, emit);
                self.next_emit = Instant::now() + INTERVAL;
            }
        }
        if terminal {
            let tails = std::mem::take(&mut self.tails);
            self.emit(tails, emit);
            self.stop();
        } else if Instant::now() >= self.next_tick {
            if let Some(tick) = &self.tick {
                let _ = tick.try_send(());
            }
            self.next_tick = Instant::now() + INTERVAL;
        }
    }

    fn emit(&mut self, lines: Vec<(&'static str, String)>, emit: &mut impl FnMut(ProgressFrame)) {
        let mut chunk = String::new();
        let mut chunk_kind = "stdout";
        for (kind, line) in lines {
            if chunk_kind != kind || chunk.len() + line.len() > FRAME_BYTES {
                self.send(chunk_kind, std::mem::take(&mut chunk), emit);
            }
            chunk_kind = kind;
            chunk.push_str(&line);
        }
        self.send(chunk_kind, chunk, emit);
    }

    fn send(&mut self, kind: &str, chunk: String, emit: &mut impl FnMut(ProgressFrame)) {
        if chunk.is_empty() || self.tick.is_none() {
            return;
        }
        if chunk.len() > CALL_BYTES - self.emitted {
            self.stop();
            return;
        }
        self.emitted += chunk.len();
        emit(ProgressFrame::new(
            &self.request_id,
            if kind == "stderr" {
                ProgressKind::Stderr
            } else {
                ProgressKind::Stdout
            },
            chunk,
        ));
        if self.emitted == CALL_BYTES {
            self.stop();
        }
    }

    fn stop(&mut self) {
        self.tick.take();
        self.ready.take();
        self.tails.clear();
    }
}

fn read_batch(
    registry: &BgTaskRegistry,
    task_id: &str,
    session_id: &str,
    streams: &mut [StreamCursor; 2],
) -> Batch {
    let mut batch = Batch::default();
    for ((artifact, kind), cursor) in [
        (TaskArtifact::Stdout, "stdout"),
        (TaskArtifact::Stderr, "stderr"),
    ]
    .into_iter()
    .zip(streams)
    {
        let Ok(page) = registry.read_artifact_page(task_id, session_id, artifact, cursor.offset)
        else {
            continue;
        };
        if page.next_offset < cursor.offset {
            *cursor = StreamCursor {
                skipping_line: true,
                ..StreamCursor::default()
            };
            continue;
        }
        let bytes = &page.bytes[..page.bytes.len().min(FRAME_BYTES)];
        cursor.offset += bytes.len() as u64;
        let mask = |from, text: &mut String| {
            *text = registry.masked_status_page(task_id, artifact, from, text.as_bytes());
        };
        batch.lines.extend(
            cursor
                .accept(bytes, false, &mask)
                .into_iter()
                .map(|text| (kind, text)),
        );
        if let Some(tail) = cursor.tail(&mask) {
            batch.tails.push((kind, tail));
        }
    }
    batch
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_progress_holds_partial_utf8_lines_and_masks_before_emission() {
        let mut cursor = StreamCursor::default();
        let mask = |_, text: &mut String| *text = text.replace("secret-value", "[REDACTED]");
        assert!(cursor.accept(b"secret-", false, &mask).is_empty());
        assert!(cursor.accept(b"value \xc3", false, &mask).is_empty());
        assert_eq!(
            cursor.accept(b"\xa9\nlast", false, &mask),
            ["[REDACTED] é\n"]
        );
        assert_eq!(cursor.tail(&mask).as_deref(), Some("last"));
        assert_eq!(cursor.accept(b" line\n", false, &mask), ["last line\n"]);
        assert!(cursor.tail(&mask).is_none());
    }

    #[test]
    fn bash_progress_withholds_private_keys_and_oversized_lines() {
        let mut cursor = StreamCursor::default();
        let mask = |_, _: &mut String| {};
        assert!(cursor
            .accept(b"-----BEGIN RSA PRIVATE KEY-----\nsecret\n", false, &mask)
            .is_empty());
        assert!(cursor.tail(&mask).is_none());
        assert_eq!(
            cursor.accept(b"-----END RSA PRIVATE KEY-----\nok\n", false, &mask),
            ["[REDACTED]\n", "ok\n"]
        );
        assert!(cursor
            .accept(&vec![b'x'; FRAME_BYTES + 1], false, &mask)
            .is_empty());
        assert_eq!(cursor.accept(b"tail\nsafe\n", false, &mask), ["safe\n"]);
        assert_eq!(cursor.accept(b"cut-secret\nnew\n", true, &mask), ["new\n"]);
    }

    #[test]
    fn bash_progress_stops_silently_at_one_megabyte() {
        let mut frames = Vec::new();
        let (tick, _ticks) = mpsc::sync_channel(1);
        let (_results, ready) = mpsc::sync_channel(1);
        let mut pump = BashProgressPump {
            tick: Some(tick),
            ready: Some(ready),
            request_id: "cap".into(),
            next_tick: Instant::now(),
            next_emit: Instant::now(),
            tails: Vec::new(),
            emitted: 0,
        };
        let line = format!("{}\n", "x".repeat(FRAME_BYTES - 1));
        for _ in 0..300 {
            pump.emit(vec![("stdout", line.clone())], &mut |frame| {
                frames.push(frame)
            });
        }
        assert_eq!(frames.len(), 256);
        assert_eq!(
            frames.iter().map(|frame| frame.chunk.len()).sum::<usize>(),
            CALL_BYTES
        );
        assert!(frames
            .iter()
            .all(|frame| frame.chunk.len() <= FRAME_BYTES && frame.chunk.ends_with('\n')));
        assert!(pump.tick.is_none());
    }

    #[test]
    fn bash_progress_terminal_poll_does_not_wait_for_reader() {
        let (tick, _ticks) = mpsc::sync_channel(1);
        let (_results, ready) = mpsc::sync_channel(1);
        let mut pump = BashProgressPump {
            tick: Some(tick),
            ready: Some(ready),
            request_id: "slow".into(),
            next_tick: Instant::now(),
            next_emit: Instant::now(),
            tails: Vec::new(),
            emitted: 0,
        };
        pump.poll(false, &mut |_| {});
        pump.poll(true, &mut |_| {});
        assert!(pump.tick.is_none());
    }
}
