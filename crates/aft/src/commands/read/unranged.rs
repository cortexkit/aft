use super::{is_binary, render_text_read, Path, RawRequest, Response, MAX_BYTES, MAX_LINE_LENGTH};

const CHUNK_BYTES: usize = 16 * 1024;
// Keep enough bytes past the display cutoff to recognize a truncated line,
// even if the retained slice ends with CR or spans a four-byte character.
const LINE_PREFIX_BYTES: usize = MAX_LINE_LENGTH + 5;

pub(super) fn count_lines(bytes: &[u8]) -> u32 {
    memchr::memchr_iter(b'\n', bytes).count() as u32
        + u32::from(bytes.last().is_some_and(|&last| last != b'\n'))
}

pub(super) fn read(req: &RawRequest, path: &Path, limit: u32) -> Response {
    scan(req, path, limit).unwrap_or_else(|error| {
        Response::error(
            &req.id,
            "io_error",
            format!("read: failed to read file: {error}"),
        )
    })
}

fn scan(req: &RawRequest, path: &Path, limit: u32) -> std::io::Result<Response> {
    let mut reader = crate::bounded_io::reader(path, None)?;
    let mut buffer = reader.chunk(Vec::new(), CHUNK_BYTES)?;
    let binary = is_binary(&buffer);
    let mut invalid_utf8 = false;
    let mut byte_size = 0;
    let mut newlines = 0;
    let mut final_byte = None;
    let mut carry = 0;
    let mut prefix = WindowPrefix {
        text: String::new(),
        lines: 0,
        line_start: 0,
        limit,
    };

    loop {
        let new_bytes = &buffer[carry..];
        if new_bytes.is_empty() {
            invalid_utf8 |= carry != 0;
            break;
        }
        byte_size += new_bytes.len();
        newlines += memchr::memchr_iter(b'\n', new_bytes).count() as u32;
        final_byte = new_bytes.last().copied();

        // Validate the entire file, including after the output window is full.
        // Carry an incomplete UTF-8 character into the next read.
        carry = 0;
        if !binary && !invalid_utf8 {
            let text = match std::str::from_utf8(&buffer) {
                Ok(text) => text,
                Err(error) if error.error_len().is_none() => {
                    carry = buffer.len() - error.valid_up_to();
                    std::str::from_utf8(&buffer[..error.valid_up_to()]).expect("valid UTF-8 prefix")
                }
                Err(_) => {
                    invalid_utf8 = true;
                    ""
                }
            };
            prefix.push(text);
        }
        #[cfg(test)]
        super::UNRANGED_READ_RETENTION.with(|retention| {
            let (_, peak_bytes) = retention.get();
            retention.set((
                prefix.lines as usize,
                peak_bytes.max(prefix.text.capacity() + buffer.capacity()),
            ));
        });
        if carry != 0 {
            let start = buffer.len() - carry;
            buffer.copy_within(start.., 0);
            buffer.truncate(carry);
            buffer = reader.append(buffer, CHUNK_BYTES)?;
        } else {
            buffer = reader.chunk(buffer, CHUNK_BYTES)?;
        }
    }

    if binary || invalid_utf8 {
        let message = if binary {
            format!("Binary file ({byte_size} bytes), cannot display as text")
        } else {
            format!("Binary file ({byte_size} bytes), not valid UTF-8")
        };
        return Ok(Response::success(
            &req.id,
            serde_json::json!({
                "binary": true, "complete": true, "byte_size": byte_size, "message": message,
            }),
        ));
    }
    let total = newlines + u32::from(final_byte.is_some_and(|last| last != b'\n'));
    Ok(render_text_read(
        req,
        &prefix.text,
        total,
        byte_size,
        1,
        None,
        limit,
        false,
    ))
}

struct WindowPrefix {
    text: String,
    lines: u32,
    line_start: usize,
    limit: u32,
}

impl WindowPrefix {
    fn push(&mut self, text: &str) {
        if self.lines >= self.limit || self.line_start > MAX_BYTES {
            return;
        }
        if self.text.capacity() == 0 && !text.is_empty() {
            self.text.reserve_exact(MAX_BYTES + LINE_PREFIX_BYTES + 1);
        }
        for part in text.split_inclusive('\n') {
            let newline = part.ends_with('\n');
            let line = part.strip_suffix('\n').unwrap_or(part);
            let room = LINE_PREFIX_BYTES - (self.text.len() - self.line_start);
            let end = line.floor_char_boundary(room.min(line.len()));
            self.text.push_str(&line[..end]);
            if newline {
                self.text.push('\n');
                self.lines += 1;
                self.line_start = self.text.len();
                if self.lines >= self.limit || self.line_start > MAX_BYTES {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_review_opus_7_streamed_output_matches_full_text_at_chunk_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("text.txt");
        let req =
            serde_json::from_value(serde_json::json!({"id":"text", "command":"read"})).unwrap();
        let split_character = "a".repeat(CHUNK_BYTES - 2) + "🦀\r\n";
        let split_crlf = "x".repeat(CHUNK_BYTES - 1) + "\r\n";
        let size_cut = "🦀".repeat(501) + "\r\n";
        for content in [
            String::new(),
            "\r\n\nend\r".to_owned(),
            split_character,
            split_crlf,
            size_cut.repeat(30),
            "x\n".repeat(2050),
        ] {
            std::fs::write(&file, &content).unwrap();
            let response = read(&req, &file, 2000);
            let reference = render_text_read(
                &req,
                &content,
                content.lines().count() as u32,
                content.len(),
                1,
                None,
                2000,
                false,
            );
            assert_eq!(response.data, reference.data);
        }

        let mut invalid = "valid\n".repeat(10_000).into_bytes();
        invalid.extend([0xf0, 0x80]);
        std::fs::write(&file, &invalid).unwrap();
        assert_eq!(
            read(&req, &file, 2000).data,
            serde_json::json!({
                "binary": true, "complete": true, "byte_size": invalid.len(),
                "message": format!("Binary file ({} bytes), not valid UTF-8", invalid.len()),
            })
        );
    }
}
