use std::pin::Pin;
use std::sync::Arc;

use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder};
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{HeaderMap, Response, StatusCode};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio_util::io::StreamReader;
use zeroize::Zeroizing;

use super::{Body, State};

#[derive(Clone, Copy)]
pub(super) enum Coding {
    Identity,
    Gzip,
    Deflate,
    Brotli,
}

pub(super) fn io_error() -> std::io::Error {
    std::io::Error::other(super::PROXY_REFUSAL)
}

pub(super) fn strip_hop_headers(headers: &mut HeaderMap, upgrade: bool) {
    let named: Vec<_> = headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|v| v.trim().to_owned())
        .collect();
    for name in named {
        if !upgrade || !name.eq_ignore_ascii_case("upgrade") {
            headers.remove(name);
        }
    }
    for name in [
        "proxy-authorization",
        "proxy-authenticate",
        "proxy-connection",
        "keep-alive",
        "te",
        "transfer-encoding",
    ] {
        headers.remove(name);
    }
    if !upgrade {
        headers.remove("connection");
        headers.remove("upgrade");
    }
}

fn scrub_headers(headers: &mut HeaderMap, state: &State, upgrade: bool) -> Result<(), ()> {
    // Names are protocol identifiers, not an allowed secret location. A server
    // that puts held material in a name gets refused instead of echoing it.
    for name in headers.keys() {
        if state.scrub(name.as_str().as_bytes())? != name.as_str().as_bytes() {
            return Err(());
        }
    }
    for value in headers.values_mut() {
        let bytes = state.scrub(value.as_bytes())?;
        *value = hyper::header::HeaderValue::from_bytes(&bytes).map_err(|_| ())?;
    }
    strip_hop_headers(headers, upgrade);
    headers.remove("content-length");
    Ok(())
}

pub(super) async fn forward(
    mut response: Response<Incoming>,
    state: Arc<State>,
    client_upgrade: Option<hyper::upgrade::OnUpgrade>,
    lease: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Response<Body>, ()> {
    // Hyper removes chunk framing, not arbitrary preceding transfer codings.
    // Do not discard that declaration and pass its still-encoded bytes onward.
    let mut transfers = response.headers().get_all("transfer-encoding").iter();
    if transfers.next().is_some_and(|v| {
        v.to_str()
            .map_or(true, |s| !s.trim().eq_ignore_ascii_case("chunked"))
    }) || transfers.next().is_some()
    {
        return Err(());
    }
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        if client_upgrade.is_none()
            || response.headers().contains_key("sec-websocket-extensions")
            || !response
                .headers()
                .get("upgrade")
                .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
        {
            return Err(());
        }
        scrub_headers(response.headers_mut(), &state, true)?;
        let upstream_upgrade = hyper::upgrade::on(&mut response);
        let client_upgrade = client_upgrade.ok_or(())?;
        tokio::spawn(async move {
            let _lease = lease;
            if let (Ok(client), Ok(upstream)) = tokio::join!(client_upgrade, upstream_upgrade) {
                super::websocket::relay(client, upstream, state).await;
            }
        });
        let (mut parts, _) = response.into_parts();
        parts.extensions.clear();
        return Ok(Response::from_parts(parts, super::full(Bytes::new())));
    }
    let encodings = content_codings(response.headers())?;
    scrub_headers(response.headers_mut(), &state, false)?;
    response.headers_mut().remove("content-encoding");
    let (mut parts, incoming) = response.into_parts();
    parts.extensions.clear();
    let trailers = Arc::new(Mutex::new(None));
    let captured = trailers.clone();
    let incoming = Arc::new(tokio::sync::Mutex::new(incoming));
    let remaining = incoming.clone();
    let stream =
        futures_util::stream::unfold((incoming, captured), |(incoming, captured)| async move {
            loop {
                let frame = incoming.lock().await.frame().await;
                match frame {
                    Some(Ok(frame)) => match frame.into_data() {
                        Ok(bytes) => {
                            return Some((Ok::<_, std::io::Error>(bytes), (incoming, captured)))
                        }
                        Err(frame) => {
                            if let Ok(headers) = frame.into_trailers() {
                                *captured.lock() = Some(headers);
                            }
                        }
                    },
                    Some(Err(_)) => return Some((Err(io_error()), (incoming, captured))),
                    None => return None,
                }
            }
        })
        .boxed();
    let mut reader: Pin<Box<dyn AsyncRead + Send>> = Box::pin(StreamReader::new(stream));
    for encoding in encodings.iter().rev() {
        reader = match encoding {
            Coding::Gzip => {
                let mut decoder = GzipDecoder::new(BufReader::new(reader));
                decoder.multiple_members(true);
                Box::pin(decoder)
            }
            Coding::Deflate => Box::pin(ZlibDecoder::new(BufReader::new(reader))),
            Coding::Brotli => Box::pin(BrotliDecoder::new(BufReader::new(reader))),
            Coding::Identity => reader,
        };
    }
    let mut scrubber = state.scrubber()?;
    let (sender, receiver) = tokio::sync::mpsc::channel(2);
    tokio::spawn(async move {
        let _lease = lease;
        let run = async {
            let mut buffer = Zeroizing::new(vec![0u8; 8192]);
            loop {
                let count = reader.read(&mut buffer).await.map_err(|_| ())?;
                state.refresh(&mut scrubber)?;
                if count == 0 {
                    break;
                }
                let clean = scrubber.push(&buffer[..count]);
                if !clean.is_empty() {
                    sender
                        .send(Ok(Frame::data(Bytes::from(clean))))
                        .await
                        .map_err(|_| ())?;
                }
            }
            // A decoder can stop at its footer before polling HTTP trailers.
            // Drain the shared input explicitly rather than silently dropping
            // those trailers with the decoder.
            drop(reader);
            while let Some(frame) = remaining.lock().await.frame().await {
                let frame = frame.map_err(|_| ())?;
                if frame.data_ref().is_some_and(|data| !data.is_empty()) {
                    return Err(());
                }
                if let Ok(headers) = frame.into_trailers() {
                    *trailers.lock() = Some(headers);
                }
            }
            let clean = scrubber.finish();
            if !clean.is_empty() {
                sender
                    .send(Ok(Frame::data(Bytes::from(clean))))
                    .await
                    .map_err(|_| ())?;
            }
            let trailer_headers = trailers.lock().take();
            if let Some(mut headers) = trailer_headers {
                scrub_headers(&mut headers, &state, false)?;
                sender
                    .send(Ok(Frame::trailers(headers)))
                    .await
                    .map_err(|_| ())?;
            }
            Ok::<_, ()>(())
        }
        .await;
        if run.is_err() {
            let _ = sender.send(Err(io_error())).await;
        }
    });
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|frame| (frame, receiver))
    });
    Ok(Response::from_parts(
        parts,
        StreamBody::new(stream).boxed_unsync(),
    ))
}

pub(super) fn content_codings(headers: &HeaderMap) -> Result<Vec<Coding>, ()> {
    let mut encodings = Vec::new();
    for value in headers.get_all("content-encoding") {
        for token in value.to_str().map_err(|_| ())?.split(',') {
            let token = token.trim();
            let coding = if token.eq_ignore_ascii_case("identity") {
                Coding::Identity
            } else if token.eq_ignore_ascii_case("gzip") {
                Coding::Gzip
            } else if token.eq_ignore_ascii_case("deflate") {
                Coding::Deflate
            } else if token.eq_ignore_ascii_case("br") {
                Coding::Brotli
            } else {
                return Err(());
            };
            if encodings.len() >= super::limits::CODINGS {
                return Err(());
            }
            encodings.push(coding);
        }
    }
    Ok(encodings)
}
