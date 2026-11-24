//! Shared worker read broker client and wire protocol.

#[cfg(unix)]
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(unix)]
use {
    crate::gh_shim_relay::RelayReply,
    serde_json::json,
    tokio::io::{AsyncReadExt, AsyncWriteExt},
};

#[doc(hidden)]
pub const SOCKET_ENV: &str = "AFT_GH_READ_SOCKET";
#[doc(hidden)]
pub const CAPABILITY_FILE_ENV: &str = "AFT_GH_READ_CAPABILITY_FILE";
#[doc(hidden)]
pub const STATE_ENV: &str = "AFT_GH_SHIM_STATE_DIR";
#[doc(hidden)]
pub const CHILD_KEYS: [&str; 5] = [
    crate::gh_shim_paths::GH_SHIM_TICKET_ENV,
    crate::gh_shim_paths::GH_SHIM_WORKER_ENV,
    SOCKET_ENV,
    CAPABILITY_FILE_ENV,
    STATE_ENV,
];
#[cfg(unix)]
#[doc(hidden)]
pub const MAX_FRAME: usize = 65_536;
#[cfg(unix)]
#[doc(hidden)]
pub const MAX_RESPONSE: usize = 2_097_152;
// A UTF-8 body byte can expand to six JSON bytes (for example, NUL becomes
// \u0000). Leave one request frame for the typed result's metadata and wrapper.
#[cfg(unix)]
#[doc(hidden)]
pub const MAX_ENCODED_RESPONSE: usize =
    crate::gh_shim::read_contract::MAX_REPLY_BYTES as usize * 6 + MAX_FRAME;

#[cfg(unix)]
#[doc(hidden)]
pub const CALL_DEADLINE: Duration = Duration::from_secs(20);

// Keep overload and transport failures actionable at every lookup/read boundary.
#[doc(hidden)]
pub fn failure_text(code: &str) -> String {
    match code {
        "broker_busy" => "gh read refused: too many gh commands are running at once in this shell. Retry in 1 second, or run fewer in parallel.",
        "request_rate_exceeded" => "gh read refused: more than 30 gh reads in one second. Retry in 1 second.",
        "command_budget_exceeded" => "gh read refused: this bash command has used its 600 gh reads. Start a new bash command.",
        "broker_timeout" => "gh read timed out. Retry, or run fewer gh commands in parallel.",
        "broker_unavailable" => "gh reads are unavailable in this bash command. Start a new bash command.",
        "watch_interval" => "--interval must be a positive number of seconds",
        "run_download" => "gh run download isn't available in worker shells; ask your head agent to download the artifacts",
        _ => return format!("gh read failed: {code}. Start a new bash command."),
    }.into()
}

/// Serde's closed enum rejects unknown operations and authority-bearing fields
/// before registry access. A connection contains exactly one length-prefixed JSON
/// envelope; it is not a SUBC connection and has no subscription protocol.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, tag = "op")]
#[doc(hidden)]
pub enum Operation {
    #[serde(rename = "gh_shim.session_context")]
    SessionContext,
    #[serde(rename = "gh_shim.read_request")]
    ReadRequest {
        request: crate::gh_shim::read_contract::Request,
    },
}

#[cfg(unix)]
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[doc(hidden)]
pub struct Envelope {
    pub capability: String,
    pub operation: Operation,
}

#[cfg(unix)]
#[doc(hidden)]
pub fn refusal(code: &str) -> RelayReply {
    RelayReply {
        ok: false,
        data: json!({"refusal_code": code, "stage": "worker_broker", "message": failure_text(code)}),
    }
}

#[cfg(unix)]
#[doc(hidden)]
pub fn decode(bytes: &[u8]) -> Result<Envelope, RelayReply> {
    if bytes.len() > MAX_FRAME {
        return Err(refusal("frame_too_large"));
    }
    serde_json::from_slice(bytes).map_err(|_| refusal("request_malformed"))
}

#[cfg(unix)]
#[doc(hidden)]
pub async fn write_reply<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    mut response: RelayReply,
) -> std::io::Result<()> {
    // Lean Plexus replies omit `isError`; there a missing `error` member marks
    // success, so the size cap applies to both reply forms.
    let is_error = match response.data.get("isError") {
        Some(flag) => flag != &Value::Bool(false),
        None => response.data.get("error").is_some(),
    };
    if response.ok && !is_error {
        if let Some(result) = response.data.get("result") {
            let body = result.get("body").unwrap_or(result);
            let size = match body.as_str() {
                Some(text) => text.len(),
                None => serde_json::to_vec(body)
                    .map_err(std::io::Error::other)?
                    .len(),
            };
            let cap = crate::gh_shim::read_contract::MAX_REPLY_BYTES;
            if size as u64 > cap {
                response = RelayReply {
                    ok: false,
                    data: json!({"refusal_code":"response_too_large","message":format!(
                        "worker read response is {size} bytes; cap is {cap} bytes (1 MiB); narrow the request"
                    )}),
                };
            }
        }
    }
    // Plexus also returns human-readable and structured copies of the result.
    // The worker decoder only consumes these three contract fields; carrying
    // the duplicate bodies can overflow the wire cap below the 1 MiB body cap.
    if response.data["isError"].is_boolean() {
        if let Some(data) = response.data.as_object_mut() {
            data.retain(|key, _| matches!(key.as_str(), "isError" | "result" | "error"));
        }
    }
    let body = json!({"status": if response.ok { "ok" } else { "error" }, "data": response.data});
    let mut bytes = serde_json::to_vec(&body).map_err(std::io::Error::other)?;
    if bytes.len() > MAX_ENCODED_RESPONSE {
        bytes = serde_json::to_vec(&json!({
            "status":"error",
            "data":{"refusal_code":"response_too_large","message":format!(
                "encoded worker read reply is {} bytes; wire cap is {MAX_ENCODED_RESPONSE} bytes (read body cap is {} bytes)",
                bytes.len(), crate::gh_shim::read_contract::MAX_REPLY_BYTES
            )}
        }))
        .map_err(std::io::Error::other)?;
    }
    if bytes.len() <= MAX_RESPONSE {
        return write_frame(writer, &bytes).await;
    }
    // Keep individual frames bounded while allowing the worst-case JSON
    // encoding of a valid body. Both peers enforce the aggregate byte count.
    write_frame(
        writer,
        &serde_json::to_vec(&json!({"status":"chunked","bytes":bytes.len()}))
            .map_err(std::io::Error::other)?,
    )
    .await?;
    for chunk in bytes.chunks(MAX_FRAME) {
        write_frame(writer, chunk).await?;
    }
    Ok(())
}

#[cfg(unix)]
#[doc(hidden)]
pub async fn read_reply<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> Result<Value, String> {
    let bytes = read_frame(reader, MAX_RESPONSE)
        .await
        .map_err(|error| format!("worker read broker reply failed: {error}"))?;
    let reply: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "worker read broker reply malformed".to_string())?;
    if reply["status"] != "chunked" {
        return Ok(reply);
    }
    let size = reply["bytes"]
        .as_u64()
        .ok_or_else(|| "worker read broker chunk length malformed".to_string())?;
    if size == 0 || size > MAX_ENCODED_RESPONSE as u64 {
        return Err(format!(
            "worker read broker chunked reply is {size} bytes; cap is {MAX_ENCODED_RESPONSE} bytes"
        ));
    }
    let mut bytes = Vec::with_capacity(size as usize);
    while bytes.len() < size as usize {
        let chunk = read_frame(reader, MAX_FRAME.min(size as usize - bytes.len()))
            .await
            .map_err(|error| format!("worker read broker reply chunk failed: {error}"))?;
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| "worker read broker reply malformed".to_string())
}

#[cfg(unix)]
#[doc(hidden)]
pub async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<Vec<u8>> {
    let size = reader.read_u32().await? as usize;
    if size == 0 || size > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("broker frame is {size} bytes; cap is {cap} bytes (size must be nonzero)"),
        ));
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

#[cfg(unix)]
#[doc(hidden)]
pub async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
) -> std::io::Result<()> {
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(bytes).await
}

/// Client discovery never opens user configuration or a general connection file.
/// In worker mode, absent paths are an error rather than a discovery fallback.
#[derive(Clone, Debug)]
#[doc(hidden)]
pub enum ClientError {
    #[cfg(not(unix))]
    UnsupportedPlatform(&'static str),
    #[cfg(unix)]
    MissingPath(&'static str),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(not(unix))]
            Self::UnsupportedPlatform(platform) => write!(f, "gh_shim_worker_read_platform_unsupported_{platform}: worker read broker is unavailable on {platform}"),
            #[cfg(unix)]
            Self::MissingPath(key) => write!(f, "worker read broker requires {key}. Start a new bash command."),
        }
    }
}

#[doc(hidden)]
pub struct Client {
    #[cfg(unix)]
    socket: PathBuf,
    #[cfg(unix)]
    capability_file: PathBuf,
}
impl Client {
    pub fn from_process() -> Result<Self, ClientError> {
        #[cfg(not(unix))]
        return Err(ClientError::UnsupportedPlatform(std::env::consts::OS));
        #[cfg(unix)]
        {
            let path = |key: &'static str| {
                std::env::var_os(key)
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .ok_or(ClientError::MissingPath(key))
            };
            Ok(Self {
                socket: path(SOCKET_ENV)?,
                capability_file: path(CAPABILITY_FILE_ENV)?,
            })
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    #[test]
    fn gh_shim_broker_windows_has_a_typed_platform_refusal() {
        assert!(matches!(
            Client::from_process(),
            Err(ClientError::UnsupportedPlatform("windows"))
        ));
    }
}

#[doc(hidden)]
pub fn request(client: &Client, operation: Operation, budget: Duration) -> Result<Value, String> {
    #[cfg(not(unix))]
    {
        let _ = (client, operation, budget);
        Err("gh_shim_worker_read_platform_unsupported_windows".into())
    }
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&client.capability_file)
            .map_err(|_| failure_text("capability_file_unavailable"))?;
        if !file
            .metadata()
            .map_err(|_| failure_text("capability_metadata_unavailable"))?
            .is_file()
        {
            return Err(failure_text("capability_not_regular_file"));
        }
        let mut capability = String::new();
        file.by_ref()
            .take(33)
            .read_to_string(&mut capability)
            .map_err(|_| failure_text("capability_unavailable"))?;
        if capability.len() != 32 || !capability.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(failure_text("capability_malformed"));
        }
        let bytes = serde_json::to_vec(&Envelope {
            capability,
            operation,
        })
        .map_err(|_| failure_text("request_malformed"))?;
        if bytes.len() > MAX_FRAME {
            return Err(failure_text("request_too_large"));
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| failure_text(&e.to_string()))?;
        runtime.block_on(async {
            tokio::time::timeout(budget.min(CALL_DEADLINE), async {
                let mut stream = tokio::net::UnixStream::connect(&client.socket)
                    .await
                    .map_err(|_| failure_text("broker_unavailable"))?;
                let peer = stream
                    .peer_cred()
                    .map_err(|_| failure_text("peer_unavailable"))?;
                if peer.uid() != unsafe { libc::geteuid() } {
                    return Err(failure_text("peer_mismatch"));
                }
                write_frame(&mut stream, &bytes)
                    .await
                    .map_err(|_| failure_text("broker_unavailable"))?;
                let reply = read_reply(&mut stream)
                    .await
                    .map_err(|error| failure_text(&error))?;
                match reply["status"].as_str() {
                    Some("ok") => reply
                        .get("data")
                        .cloned()
                        .ok_or_else(|| failure_text("reply_omitted_data")),
                    Some("error") => {
                        // Keep the daemon's explanation (for example, which
                        // query keys are allowed) so a refused command can be
                        // corrected without inspecting broker traffic.
                        let code = reply["data"]["refusal_code"].as_str().unwrap_or("refused");
                        Err(match reply["data"]["message"].as_str() {
                            Some(message) if reply["data"]["stage"] == "worker_broker" => {
                                message.to_string()
                            }
                            Some(message) if !message.is_empty() => {
                                format!("worker read broker refused: {code}: {message}")
                            }
                            _ => failure_text(code),
                        })
                    }
                    _ => Err(failure_text("reply_unknown_status")),
                }
            })
            .await
            .map_err(|_| failure_text("broker_timeout"))?
        })
    }
}
