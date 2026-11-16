//! Compatibility plumbing shared by framed fake subc daemons.

use subc_protocol::{Frame, FrameType};
use tokio::io::{AsyncRead, AsyncWrite};

/// These daemons have no bus module. Refuse `ckbus` discovery requests while
/// preserving their correlation, leaving other route opens for the test's assertions.
pub(crate) fn ckbus_route_refusal(request: &Frame) -> Option<Frame> {
    if request.header.ty != FrameType::Request || request.header.channel != 0 {
        return None;
    }
    let body: serde_json::Value = serde_json::from_slice(&request.body).ok()?;
    if body["op"] != "route.open"
        || body["target"] != serde_json::json!({"kind": "management_surface", "module_id": "ckbus"})
    {
        return None;
    }
    Some(consumer_error(request, "target_unavailable"))
}

fn consumer_error(request: &Frame, code: &str) -> Frame {
    Frame::build_with_version(
        request.header.ver,
        FrameType::Error,
        request.header.flags,
        request.header.channel,
        request.header.epoch,
        request.header.corr,
        serde_json::to_vec(&serde_json::json!({"code": code, "message": code})).unwrap(),
    )
    .unwrap()
}

/// Serve an auxiliary consumer without advertising optional modules or catalog
/// watches. Keep the connection alive after a refused route open, as the daemon does.
pub(crate) async fn serve_without_modules(mut stream: tokio::net::TcpStream, mut request: Frame) {
    loop {
        if request.header.ty == FrameType::Goodbye {
            return;
        }
        let reply = server_describe_reply(&request)
            .or_else(|| ckbus_route_refusal(&request))
            .unwrap_or_else(|| {
                let body: serde_json::Value =
                    serde_json::from_slice(&request.body).unwrap_or_default();
                if request.header.ty == FrameType::Request
                    && request.header.channel == 0
                    && body["op"] == "route.open"
                {
                    consumer_error(&request, "target_unavailable")
                } else {
                    consumer_error(&request, "unknown_op")
                }
            });
        if subc_transport::write_frame(&mut stream, &reply)
            .await
            .is_err()
        {
            return;
        }
        match subc_transport::read_frame(&mut stream).await {
            Ok(Some(frame)) => request = frame,
            _ => return,
        }
    }
}

/// Answer the SDK's advisory probe without advertising new features: these
/// fakes implement neither reason-bearing route closes nor other optional
/// daemon capabilities, and must keep exercising the legacy route behavior.
pub(crate) fn server_describe_reply(request: &Frame) -> Option<Frame> {
    if request.header.ty != FrameType::Request || request.header.channel != 0 {
        return None;
    }
    let body: serde_json::Value = serde_json::from_slice(&request.body).ok()?;
    if body["op"] != "server.describe" {
        return None;
    }
    Some(
        Frame::build_with_version(
            request.header.ver,
            FrameType::Response,
            request.header.flags,
            request.header.channel,
            request.header.epoch,
            request.header.corr,
            serde_json::to_vec(&serde_json::json!({
                "op": "server.describe", "protocol_ver": 2,
                "subc_ops": [], "capabilities": [], "connected_clients": 1,
            }))
            .unwrap(),
        )
        .unwrap(),
    )
}

/// Read the next scripted frame, answering advisory probes along the way so
/// exact assertions about application requests remain unchanged.
pub(crate) async fn read_frame<S>(
    stream: &mut S,
) -> Result<Option<Frame>, subc_transport::FrameIoError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let Some(frame) = subc_transport::read_frame(stream).await? else {
            return Ok(None);
        };
        if let Some(reply) = server_describe_reply(&frame) {
            subc_transport::write_frame(stream, &reply).await?;
        } else {
            return Ok(Some(frame));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_refusal_preserves_correlation_and_does_not_hide_other_route_opens() {
        for module in ["ckbus", "claustrum", "prefrontal-core"] {
            let request = Frame::build(
                FrameType::Request,
                subc_protocol::Flags::new(false, subc_protocol::Priority::Interactive, false),
                0,
                0,
                73,
                serde_json::to_vec(&serde_json::json!({
                    "op": "route.open",
                    "target": {"kind": "management_surface", "module_id": module}
                }))
                .unwrap(),
            )
            .unwrap();
            let refusal = ckbus_route_refusal(&request);
            if module != "ckbus" {
                assert!(refusal.is_none());
                continue;
            }
            let refusal = refusal.expect("bus module is absent");
            assert_eq!(refusal.header.ty, FrameType::Error);
            assert_eq!(refusal.header.ver, request.header.ver);
            assert_eq!(refusal.header.channel, request.header.channel);
            assert_eq!(refusal.header.epoch, request.header.epoch);
            assert_eq!(refusal.header.corr, request.header.corr);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&refusal.body).unwrap()["code"],
                "target_unavailable"
            );
        }
    }
}
