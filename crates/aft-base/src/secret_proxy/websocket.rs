use std::sync::Arc;

use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

use super::State;

pub(super) async fn relay(
    client: hyper::upgrade::Upgraded,
    upstream: hyper::upgrade::Upgraded,
    state: Arc<State>,
) {
    let (mut client_read, mut client_write) = tokio::io::split(TokioIo::new(client));
    let (mut upstream_read, mut upstream_write) = tokio::io::split(TokioIo::new(upstream));
    // Client frames are never substituted. In particular, a placeholder in a
    // data frame is not a second handshake credential location.
    tokio::select! {
        _ = tokio::io::copy(&mut client_read, &mut upstream_write) => {},
        _ = server_frames(&mut upstream_read, &mut client_write, &state) => {},
    }
}

async fn server_frames<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    state: &State,
) -> Result<(), ()> {
    let mut active: Option<(u8, secret_rules::scrub::Scrubber, bool)> = None;
    let mut buffer = Zeroizing::new(vec![0u8; 8192]);
    loop {
        let mut header = [0u8; 2];
        reader.read_exact(&mut header).await.map_err(|_| ())?;
        // Reject reserved bits before reading or forwarding the frame payload.
        if header[0] & 0x70 != 0 || header[1] & 0x80 != 0 {
            return Err(());
        }
        let fin = header[0] & 0x80 != 0;
        let kind = header[0] & 0x0f;
        let length = match header[1] & 0x7f {
            126 => reader.read_u16().await.map_err(|_| ())? as u64,
            127 => reader.read_u64().await.map_err(|_| ())?,
            size => size as u64,
        };
        if length > i64::MAX as u64 {
            return Err(());
        }
        if kind >= 8 {
            if !matches!(kind, 8..=10) || !fin || length > 125 {
                return Err(());
            }
            reader
                .read_exact(&mut buffer[..length as usize])
                .await
                .map_err(|_| ())?;
            let clean = state.scrub(&buffer[..length as usize])?;
            if clean.len() > 125 {
                return Err(());
            }
            write_frame(writer, kind, true, &clean).await?;
            if kind == 8 {
                return Ok(());
            }
            continue;
        }
        match kind {
            1 | 2 if active.is_none() => active = Some((kind, state.scrubber()?, false)),
            0 if active.is_some() => {}
            _ => return Err(()),
        }
        let (opcode, scrubber, started) = active.as_mut().ok_or(())?;
        let mut remaining = length;
        // Emit new fragments from one per-message scrubber. Its overlap spans
        // both transport reads and frame boundaries without buffering a whole
        // message or ever mixing bytes from separate WebSocket messages.
        while remaining > 0 {
            let count = remaining.min(buffer.len() as u64) as usize;
            reader
                .read_exact(&mut buffer[..count])
                .await
                .map_err(|_| ())?;
            remaining -= count as u64;
            state.refresh(scrubber)?;
            let clean = scrubber.push(&buffer[..count]);
            if !clean.is_empty() {
                write_frame(writer, if *started { 0 } else { *opcode }, false, &clean).await?;
                *started = true;
            }
        }
        if fin {
            let (opcode, mut scrubber, started) = active.take().ok_or(())?;
            state.refresh(&mut scrubber)?;
            write_frame(
                writer,
                if started { 0 } else { opcode },
                true,
                &scrubber.finish(),
            )
            .await?;
        }
    }
}

async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    opcode: u8,
    fin: bool,
    payload: &[u8],
) -> Result<(), ()> {
    writer
        .write_u8((if fin { 0x80 } else { 0 }) | opcode)
        .await
        .map_err(|_| ())?;
    match payload.len() {
        0..=125 => writer.write_u8(payload.len() as u8).await.map_err(|_| ())?,
        126..=65535 => {
            writer.write_u8(126).await.map_err(|_| ())?;
            writer
                .write_u16(payload.len() as u16)
                .await
                .map_err(|_| ())?;
        }
        _ => {
            writer.write_u8(127).await.map_err(|_| ())?;
            writer
                .write_u64(payload.len() as u64)
                .await
                .map_err(|_| ())?;
        }
    }
    writer.write_all(payload).await.map_err(|_| ())?;
    writer.flush().await.map_err(|_| ())
}
