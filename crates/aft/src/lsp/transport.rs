use std::io::{self, BufRead, Write};

use crate::lsp::jsonrpc::{Notification, Request, ServerMessage};

const MAX_MESSAGE_SIZE: usize = 10 * 1024 * 1024;

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Read a single LSP message from a buffered reader.
/// Format: "Content-Length: N\r\n\r\n{json body of N bytes}"
pub fn read_message(reader: &mut impl BufRead) -> io::Result<Option<ServerMessage>> {
    let mut content_length = None;
    let mut saw_header = false;

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line)?;

        if bytes_read == 0 {
            return if saw_header {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unexpected EOF while reading LSP headers",
                ))
            } else {
                Ok(None)
            };
        }

        if !line.ends_with('\n') {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected EOF while reading LSP headers",
            ));
        }

        if line == "\r\n" || line == "\n" {
            break;
        }

        saw_header = true;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let (name, value) = trimmed
            .split_once(':')
            .ok_or_else(|| invalid_data("malformed header"))?;

        if name.eq_ignore_ascii_case("Content-Length") {
            let parsed = value
                .trim()
                .parse::<usize>()
                .map_err(|_| invalid_data("malformed Content-Length header"))?;

            if parsed > MAX_MESSAGE_SIZE {
                return Err(invalid_data("LSP message exceeds maximum size"));
            }

            content_length = Some(parsed);
        }
    }

    let content_length =
        content_length.ok_or_else(|| invalid_data("missing Content-Length header"))?;
    let mut body = vec![0_u8; content_length];

    reader.read_exact(&mut body).map_err(|err| {
        if err.kind() == io::ErrorKind::UnexpectedEof {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected EOF while reading LSP body",
            )
        } else {
            err
        }
    })?;

    let json =
        String::from_utf8(body).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

    ServerMessage::from_json(&json)
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn write_message(writer: &mut impl Write, payload: &str) -> io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", payload.len())?;
    writer.write_all(payload.as_bytes())?;
    writer.flush()
}

/// Write a JSON-RPC request to a writer with Content-Length framing.
pub fn write_request(writer: &mut impl Write, request: &Request) -> io::Result<()> {
    let json = serde_json::to_string(request)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    write_message(writer, &json)
}

/// Write a JSON-RPC notification to a writer with Content-Length framing.
pub fn write_notification(writer: &mut impl Write, notification: &Notification) -> io::Result<()> {
    let json = serde_json::to_string(notification)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    write_message(writer, &json)
}

/// Write a JSON-RPC response to a writer with Content-Length framing.
/// Used to respond to server-initiated requests (e.g. client/registerCapability).
pub fn write_response(
    writer: &mut impl Write,
    response: &super::jsonrpc::OutgoingResponse,
) -> io::Result<()> {
    let json = serde_json::to_string(response)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    write_message(writer, &json)
}
