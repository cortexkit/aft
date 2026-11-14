use std::panic::{catch_unwind, AssertUnwindSafe};

const FALLBACK_TAIL_BYTES: usize = 16 * 1024;

pub fn render_screen(raw: &[u8], rows: u16, cols: u16) -> String {
    catch_unwind(AssertUnwindSafe(|| render_screen_inner(raw, rows, cols)))
        .unwrap_or_else(|_| render_raw_fallback(raw))
}

fn render_screen_inner(raw: &[u8], rows: u16, cols: u16) -> String {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(raw);
    let screen = parser.screen();
    let mut lines: Vec<String> = Vec::new();
    for y in 0..rows {
        let mut text = String::new();
        for x in 0..cols {
            let cell = screen.cell(y, x);
            // A wide glyph already occupies its continuation column.
            if cell.is_some_and(vt100::Cell::is_wide_continuation) {
                continue;
            }
            let c = cell.map(|cell| cell.contents()).unwrap_or_default();
            if c.is_empty() {
                text.push(' ');
            } else {
                text.push_str(c);
            }
        }
        lines.push(text.trim_end().to_string());
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

pub fn render_raw_fallback(raw: &[u8]) -> String {
    let tail = if raw.len() > FALLBACK_TAIL_BYTES {
        &raw[raw.len() - FALLBACK_TAIL_BYTES..]
    } else {
        raw
    };
    let note = if raw.len() > FALLBACK_TAIL_BYTES {
        format!("[PTY screen render panicked; showing last {FALLBACK_TAIL_BYTES} raw bytes]\n")
    } else {
        "[PTY screen render panicked; showing raw PTY bytes]\n".to_string()
    };
    format!("{note}{}", String::from_utf8_lossy(tail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_glyphs_do_not_insert_spaces_before_following_text() {
        assert_eq!(render_screen("界a 😀b".as_bytes(), 2, 20), "界a 😀b");
        assert_eq!(render_screen("界\x1b[2Cx".as_bytes(), 2, 20), "界  x");
    }

    const CASES: &[(&str, &[u8], &str)] = &[
        (
            "cap1_ls",
            include_bytes!("../../aft/tests/fixtures/pty_render/cap1_ls.raw"),
            include_str!("../../aft/tests/fixtures/pty_render/cap1_ls.xterm.txt"),
        ),
        (
            "cap2_sgr",
            include_bytes!("../../aft/tests/fixtures/pty_render/cap2_sgr.raw"),
            include_str!("../../aft/tests/fixtures/pty_render/cap2_sgr.xterm.txt"),
        ),
        (
            "cap3_box",
            include_bytes!("../../aft/tests/fixtures/pty_render/cap3_box.raw"),
            include_str!("../../aft/tests/fixtures/pty_render/cap3_box.xterm.txt"),
        ),
        (
            "cap4_cr",
            include_bytes!("../../aft/tests/fixtures/pty_render/cap4_cr.raw"),
            include_str!("../../aft/tests/fixtures/pty_render/cap4_cr.xterm.txt"),
        ),
        (
            "cap5_alt",
            include_bytes!("../../aft/tests/fixtures/pty_render/cap5_alt.raw"),
            include_str!("../../aft/tests/fixtures/pty_render/cap5_alt.xterm.txt"),
        ),
        (
            "cap6_vim",
            include_bytes!("../../aft/tests/fixtures/pty_render/cap6_vim.raw"),
            include_str!("../../aft/tests/fixtures/pty_render/cap6_vim.xterm.txt"),
        ),
    ];

    #[test]
    fn matches_xterm_headless_golden_corpus() {
        for (name, raw, expected) in CASES {
            assert_eq!(render_screen(raw, 24, 80), *expected, "{name}");
        }
    }

    #[test]
    fn panic_fallback_returns_raw_tail() {
        let raw = b"before panic";
        let rendered = catch_unwind(AssertUnwindSafe(|| {
            render_screen_catching(raw, || panic!("forced vt100 panic"))
        }))
        .expect("render_screen must catch renderer panics");
        assert!(rendered.starts_with("[PTY screen render panicked; showing raw PTY bytes]\n"));
        assert!(rendered.ends_with("before panic"));
    }

    #[test]
    fn fallback_trims_large_raw_payload_to_tail() {
        let raw = vec![b'x'; FALLBACK_TAIL_BYTES + 10];
        let rendered = render_raw_fallback(&raw);
        assert!(rendered.starts_with(&format!(
            "[PTY screen render panicked; showing last {FALLBACK_TAIL_BYTES} raw bytes]\n"
        )));
        assert_eq!(
            rendered.len(),
            format!("[PTY screen render panicked; showing last {FALLBACK_TAIL_BYTES} raw bytes]\n")
                .len()
                + FALLBACK_TAIL_BYTES
        );
    }

    fn render_screen_catching(raw: &[u8], render: impl FnOnce() -> String) -> String {
        catch_unwind(AssertUnwindSafe(render)).unwrap_or_else(|_| render_raw_fallback(raw))
    }
}
