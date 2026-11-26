//! The PTY views `bash_status` returns without `output_offset`: the terminal
//! screen and a bounded tail of the raw capture.
//!
//! A PTY capture can reach hundreds of MiB while the agent keeps polling it,
//! so neither view reads the whole capture on every poll. Each task's screen
//! keeps its terminal emulator and the capture offset it has parsed up to, so
//! a poll parses only the bytes written since the previous one. Raw output is
//! the capture's last page, the same amount one `output_offset` page returns.

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use crate::bash_background::persistence::{TaskArtifact, ValidatedArtifact};
use crate::bash_background::registry::ARTIFACT_PAGE_BYTES;
use crate::context::AppContext;

/// How many tasks' screens stay parsed. Each holds one screen of cells.
const MAX_SCREENS: usize = 32;

/// How many parsed bytes a screen remembers from just before its offset. A
/// later poll checks the capture still holds them there, so a capture that
/// was replaced rather than appended to is parsed again from its start.
const FINGERPRINT_BYTES: usize = 64;

/// Where a task's PTY capture is read from.
pub(super) enum PtyCapture<'a> {
    /// Through the task registry, which checks the caller may read the task.
    Registry {
        ctx: &'a AppContext,
        task_id: &'a str,
        session_id: &'a str,
    },
    /// A capture file opened from the task's bundle on disk.
    Disk(ValidatedArtifact),
}

impl PtyCapture<'_> {
    fn len(&mut self) -> Result<u64, String> {
        match self {
            Self::Registry {
                ctx,
                task_id,
                session_id,
            } => ctx
                .bash_background()
                .read_artifact_page(task_id, session_id, TaskArtifact::Pty, u64::MAX)
                .map(|page| page.next_offset),
            Self::Disk(file) => file.len().map_err(|error| error.to_string()),
        }
    }

    /// Up to one page of the capture from `offset`; fewer bytes when the
    /// capture ends sooner.
    fn read_page(&mut self, offset: u64) -> Result<Vec<u8>, String> {
        match self {
            Self::Registry {
                ctx,
                task_id,
                session_id,
            } => ctx
                .bash_background()
                .read_artifact_page(task_id, session_id, TaskArtifact::Pty, offset)
                .map(|page| page.bytes),
            Self::Disk(file) => file
                .read_range(offset, ARTIFACT_PAGE_BYTES as u64)
                .map_err(|error| error.to_string()),
        }
    }
}

struct Screen {
    parser: vt100::Parser,
    rows: u16,
    cols: u16,
    /// Capture bytes parsed so far.
    offset: u64,
    /// The last parsed bytes, up to [`FINGERPRINT_BYTES`].
    fingerprint: Vec<u8>,
}

impl Screen {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            rows,
            cols,
            offset: 0,
            fingerprint: Vec::new(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        self.offset += bytes.len() as u64;
        self.fingerprint
            .extend_from_slice(&bytes[bytes.len().saturating_sub(FINGERPRINT_BYTES)..]);
        let excess = self.fingerprint.len().saturating_sub(FINGERPRINT_BYTES);
        self.fingerprint.drain(..excess);
    }
}

/// Each capture's screen, with the use count of its latest poll.
#[derive(Default)]
struct Screens {
    by_capture: HashMap<String, (Arc<Mutex<Screen>>, u64)>,
    uses: u64,
}

static SCREENS: LazyLock<Mutex<Screens>> = LazyLock::new(Default::default);

/// The screen kept for `key`, created for `rows` by `cols` if there is none.
/// Evicts the least recently used screen when the cache is full.
fn screen_for(key: &str, rows: u16, cols: u16) -> Arc<Mutex<Screen>> {
    let mut screens = SCREENS.lock().unwrap_or_else(PoisonError::into_inner);
    screens.uses += 1;
    let uses = screens.uses;
    if !screens.by_capture.contains_key(key) && screens.by_capture.len() >= MAX_SCREENS {
        let oldest = screens
            .by_capture
            .iter()
            .min_by_key(|(_, (_, last_used))| *last_used)
            .map(|(key, _)| key.clone());
        if let Some(oldest) = oldest {
            screens.by_capture.remove(&oldest);
        }
    }
    let (screen, last_used) = screens
        .by_capture
        .entry(key.to_string())
        .or_insert_with(|| (Arc::new(Mutex::new(Screen::new(rows, cols))), 0));
    *last_used = uses;
    Arc::clone(screen)
}

fn forget_screen(key: &str) {
    SCREENS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .by_capture
        .remove(key);
}

/// The current screen of the capture `key` names, parsing only the bytes
/// written since the previous call for the same capture.
pub(super) fn render_screen(
    key: &str,
    capture: &mut PtyCapture<'_>,
    rows: u16,
    cols: u16,
) -> Result<String, String> {
    let screen = screen_for(key, rows, cols);
    let mut screen = screen.lock().unwrap_or_else(PoisonError::into_inner);
    if screen.rows != rows || screen.cols != cols {
        *screen = Screen::new(rows, cols);
    }
    // The first page starts at the fingerprint, so one read both confirms
    // the capture still holds the parsed bytes and returns the new ones.
    let mut page = capture.read_page(screen.offset - screen.fingerprint.len() as u64)?;
    let mut more = page.len() == ARTIFACT_PAGE_BYTES;
    if page.starts_with(&screen.fingerprint) {
        page.drain(..screen.fingerprint.len());
    } else {
        *screen = Screen::new(rows, cols);
        page = capture.read_page(0)?;
        more = page.len() == ARTIFACT_PAGE_BYTES;
    }
    loop {
        if !page.is_empty() && catch_unwind(AssertUnwindSafe(|| screen.feed(&page))).is_err() {
            drop(screen);
            forget_screen(key);
            return render_fallback(capture);
        }
        if !more {
            break;
        }
        page = capture.read_page(screen.offset)?;
        more = page.len() == ARTIFACT_PAGE_BYTES;
    }
    Ok(crate::pty_render::render_parsed_screen(
        screen.parser.screen(),
        rows,
        cols,
    ))
}

/// The raw-bytes fallback the screen renderer shows when the terminal
/// emulator panics.
fn render_fallback(capture: &mut PtyCapture<'_>) -> Result<String, String> {
    let tail = raw_tail(capture)?;
    let total = tail.omitted + tail.bytes.len() as u64;
    Ok(crate::pty_render::render_raw_tail_fallback(
        &tail.bytes,
        total as usize,
    ))
}

/// The end of a PTY capture, at most one page.
pub(super) struct RawTail {
    pub(super) bytes: Vec<u8>,
    /// Capture bytes before `bytes` that were left out.
    pub(super) omitted: u64,
}

impl RawTail {
    /// The tail as `bash_status` shows it, led by a note naming the left-out
    /// bytes when there are any.
    pub(super) fn text(&self) -> String {
        let text = String::from_utf8_lossy(&self.bytes);
        if self.omitted == 0 {
            return text.into_owned();
        }
        format!(
            "[PTY raw output: showing the last {} of {} bytes; read earlier bytes with output_offset.]\n{text}",
            self.bytes.len(),
            self.omitted + self.bytes.len() as u64
        )
    }
}

pub(super) fn raw_tail(capture: &mut PtyCapture<'_>) -> Result<RawTail, String> {
    let start = capture.len()?.saturating_sub(ARTIFACT_PAGE_BYTES as u64);
    let bytes = capture.read_page(start)?;
    Ok(RawTail {
        bytes,
        omitted: start,
    })
}
