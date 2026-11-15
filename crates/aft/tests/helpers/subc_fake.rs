//! Compatibility plumbing shared by framed fake subc daemons.

use subc_protocol::{Frame, FrameType};
use tokio::io::{AsyncRead, AsyncWrite};

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
