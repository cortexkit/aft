use super::{admission, output::Chunker, wire::*};
use crate::bash_background::persistence::TaskArtifact;
use crate::bash_background::registry::{ShellReader, SHELL_MASK_WITHHELD};
use std::time::{Duration, Instant};

pub(super) struct Capture {
    reader: ShellReader,
    task_id: String,
    stdout: Chunker,
    stderr: Chunker,
    stdout_artifact: TaskArtifact,
    prefix_known: [bool; 2],
    max: usize,
    screen: Option<Screen>,
}

struct Screen {
    parser: vt100::Parser,
    rows: u16,
    cols: u16,
    chunker: Chunker,
    available: bool,
    last_at: Instant,
    previous: Option<OutputFrame>,
}

impl Capture {
    pub(super) fn open(
        reader: ShellReader,
        row: &Row,
        params: OutputParams,
    ) -> Result<(Self, OutputFrame), subc_protocol::ErrorBody> {
        let max = params.max_chunk_bytes.unwrap_or(DEFAULT_CHUNK_BYTES);
        let ansi = params.ansi.unwrap_or_else(|| "strip".into());
        if params.from_offset.as_ref().is_some_and(|offset| {
            offset.stdout > row.output_bytes.stdout || offset.stderr > row.output_bytes.stderr
        }) {
            return Err(admission::invalid(
                "from_offset exceeds the captured stream",
            ));
        }
        let stdout_artifact = if row.kind == "pty" {
            TaskArtifact::Pty
        } else {
            TaskArtifact::Stdout
        };
        let mut starts = Offsets {
            stdout: super::output::backfill_start(
                row.output_bytes.stdout,
                params.from_offset.as_ref().map(|offset| offset.stdout),
            ),
            stderr: super::output::backfill_start(
                row.output_bytes.stderr,
                params.from_offset.as_ref().map(|offset| offset.stderr),
            ),
        };
        for (artifact, offset) in [
            (stdout_artifact, &mut starts.stdout),
            (TaskArtifact::Stderr, &mut starts.stderr),
        ] {
            if *offset > 0 {
                let edge = reader
                    .shell_read(&row.task_id, artifact, *offset, 4)
                    .unwrap_or_default();
                let continuation = edge.iter().take_while(|byte| **byte & 0xc0 == 0x80).count();
                if continuation > 0 && params.from_offset.is_some() {
                    return Err(admission::invalid("from_offset must be a UTF-8 boundary"));
                }
                *offset += continuation as u64;
            }
        }
        let mut stdout = Chunker::new(starts.stdout, max, ansi == "keep");
        let mut stderr = Chunker::new(starts.stderr, max, ansi == "keep");
        let mut prefix_known = [true; 2];
        for (index, (artifact, chunker)) in [
            (stdout_artifact, &mut stdout),
            (TaskArtifact::Stderr, &mut stderr),
        ]
        .into_iter()
        .enumerate()
        {
            let mut offset = 0;
            while offset < chunker.offset() {
                if reader.stopped() {
                    prefix_known[index] = false;
                    break;
                }
                let limit = (DEFAULT_CHUNK_BYTES as u64).min(chunker.offset() - offset);
                match reader.shell_read(&row.task_id, artifact, offset, limit) {
                    Ok(bytes) if !bytes.is_empty() => {
                        offset += bytes.len() as u64;
                        chunker.resume_escape(&bytes);
                    }
                    _ => {
                        prefix_known[index] = false;
                        break;
                    }
                }
            }
        }
        let screen = if row.kind == "pty" {
            reader
                .shell_dimensions(&row.task_id)
                .map(|(rows, cols)| Screen {
                    parser: vt100::Parser::new(rows, cols, 0),
                    rows,
                    cols,
                    chunker: Chunker::new(0, max, true),
                    available: true,
                    last_at: Instant::now() - Duration::from_secs(1),
                    previous: None,
                })
        } else {
            None
        };
        let start = OutputFrame::Start {
            task_id: row.task_id.clone(),
            ansi,
            truncated_before: (starts.stdout > 0 || starts.stderr > 0).then_some(starts),
        };
        Ok((
            Self {
                reader,
                task_id: row.task_id.clone(),
                stdout,
                stderr,
                stdout_artifact,
                prefix_known,
                max,
                screen,
            },
            start,
        ))
    }

    pub(super) fn read(mut self, row: Row) -> (Self, Vec<OutputFrame>, bool) {
        if self.reader.stopped() {
            return (self, Vec::new(), false);
        }
        let terminal = row.state == "ended";
        let longest = self.reader.shell_longest_secret(&self.task_id);
        let mut frames = Vec::new();
        for (index, (stream, artifact, chunker, length)) in [
            (
                "stdout",
                self.stdout_artifact,
                &mut self.stdout,
                row.output_bytes.stdout,
            ),
            (
                "stderr",
                TaskArtifact::Stderr,
                &mut self.stderr,
                row.output_bytes.stderr,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let pending = chunker.read_offset().saturating_sub(chunker.offset()) as usize;
            let want = (self.max + longest + 8).saturating_sub(pending);
            let bytes =
                self.reader
                    .shell_read(&self.task_id, artifact, chunker.read_offset(), want as u64);
            if !self.prefix_known[index]
                || bytes.is_err()
                || bytes
                    .as_ref()
                    .is_ok_and(|bytes| bytes.is_empty() && chunker.read_offset() < length)
            {
                if let Some(frame) = chunker.withhold_to(stream, length, SHELL_MASK_WITHHELD) {
                    frames.push(frame);
                }
                continue;
            }
            chunker.append(&bytes.unwrap());
            let at_end = terminal && chunker.read_offset() >= length;
            if let Some(frame) = chunker
                .next(stream, at_end, longest, |from, to| {
                    Ok::<_, std::convert::Infallible>(self.reader.shell_masked_range(
                        &self.task_id,
                        artifact,
                        from,
                        to,
                    ))
                })
                .unwrap()
            {
                frames.push(frame);
            }
        }
        self.read_screen(
            terminal && self.stdout.read_offset() >= row.output_bytes.stdout,
            longest,
            &mut frames,
        );
        let finished = terminal
            && self.stdout.offset() >= row.output_bytes.stdout
            && self.stderr.offset() >= row.output_bytes.stderr
            && self.screen.as_ref().is_none_or(|screen| {
                !screen.available || screen.chunker.offset() >= row.output_bytes.stdout
            });
        if finished {
            frames.push(OutputFrame::End { row });
        }
        (self, frames, finished)
    }

    fn read_screen(&mut self, terminal: bool, longest: usize, frames: &mut Vec<OutputFrame>) {
        let Some(screen) = &mut self.screen else {
            return;
        };
        if !screen.available {
            return;
        }
        if screen.last_at.elapsed() < Duration::from_millis(500) {
            return;
        }
        let end = self.stdout.read_offset();
        while screen.chunker.offset() < end {
            if self.reader.stopped() {
                break;
            }
            let pending = (screen.chunker.read_offset() - screen.chunker.offset()) as usize;
            let want = (self.max + longest + 8).saturating_sub(pending) as u64;
            if screen.chunker.read_offset() < end && want > 0 {
                let limit = want.min(end - screen.chunker.read_offset());
                match self.reader.shell_read(
                    &self.task_id,
                    self.stdout_artifact,
                    screen.chunker.read_offset(),
                    limit,
                ) {
                    Ok(bytes) if !bytes.is_empty() => screen.chunker.append(&bytes),
                    _ => {
                        screen.available = false;
                        return;
                    }
                }
            }
            let Some(OutputFrame::Chunk { text, .. }) = screen
                .chunker
                .next(
                    "stdout",
                    terminal && screen.chunker.read_offset() >= end,
                    longest,
                    |from, to| {
                        Ok::<_, std::convert::Infallible>(self.reader.shell_masked_range(
                            &self.task_id,
                            self.stdout_artifact,
                            from,
                            to,
                        ))
                    },
                )
                .unwrap()
            else {
                break;
            };
            screen.parser.process(text.as_bytes());
        }
        let text = crate::pty_render::render_screen(
            &screen.parser.screen().state_formatted(),
            screen.rows,
            screen.cols,
        );
        let (row, col) = screen.parser.screen().cursor_position();
        let snapshot = OutputFrame::Screen {
            rows: screen.rows as usize,
            cols: screen.cols as usize,
            lines: text.split('\n').map(str::to_owned).collect(),
            cursor: ScreenCursor {
                row: row as usize,
                col: col as usize,
            },
        };
        if screen.previous.as_ref() != Some(&snapshot) {
            frames.push(snapshot.clone());
            screen.previous = Some(snapshot);
        }
        screen.last_at = Instant::now();
    }
}
