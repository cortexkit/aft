use super::*;
use serde_json::{json, Value};
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// The production transport against a fake subc daemon, over real subc framing.

/// What the fake daemon does beyond opening routes and echoing a completed
/// reply to every request.
#[derive(Clone, Default)]
pub(super) struct DaemonScript {
    pub(super) reply: Option<Arc<dyn Fn(&Value, Option<&Value>) -> Value + Send + Sync>>,
    pub(super) scoped_open_pause: Option<Arc<LeaseTestPause>>,
    /// Refuse every route.open that carries a scope with this code.
    pub(super) refuse_scoped_opens: Option<String>,
    /// Before writing each scoped route.open refusal, wait this long in real
    /// time, as a slow runner would. The wait runs on a blocking thread, so
    /// it neither stalls the runtime nor moves a paused test clock.
    pub(super) scoped_refusal_delay: Option<std::time::Duration>,
    /// After each scoped route.open refusal, push a channel-0 notice with op
    /// `test.route_open_refused`. The client reads one connection's frames in
    /// order and hands each refusal to its waiting route open as it reads it,
    /// so a test that receives this notice from `SubcConsumer::control_pushes`
    /// knows the client already holds the refusal sent just before it.
    pub(super) announce_scoped_refusals: bool,
    /// After answering the first request on a scoped route, push a
    /// `route.closed` for plexus with reason `scope_ended`, and refuse every
    /// later scoped route.open with `scope_ended`, as the daemon does once a
    /// scope is removed.
    pub(super) end_scope_after_first_write: bool,
    /// Hold the reply to the first request until a second request arrives;
    /// answer the second at once and the first 200 ms later.
    pub(super) hold_first_request: bool,
    pub(super) request_reply_delay: Option<Duration>,
}

/// Everything the fake daemon was sent.
#[derive(Default)]
pub(super) struct DaemonLog {
    /// Every route.open body, refused ones included.
    pub(super) opens: Vec<Value>,
    /// The channel of every route the client closed.
    pub(super) closes: Vec<Value>,
    /// Every request body sent on an open route.
    pub(super) requests: Vec<Value>,
    pub(super) request_channels: Vec<u16>,
    pub(super) connections_closed: usize,
}

struct DaemonConnection(Arc<Mutex<DaemonLog>>);

impl Drop for DaemonConnection {
    fn drop(&mut self) {
        self.0.lock().unwrap().connections_closed += 1;
    }
}

pub struct FakeDaemon {
    pub(super) log: Arc<Mutex<DaemonLog>>,
    pub(super) connection_file: std::path::PathBuf,
    pub(super) _dir: tempfile::TempDir,
}

impl FakeDaemon {
    pub fn opens(&self) -> Vec<Value> {
        self.log.lock().unwrap().opens.clone()
    }

    /// The routes closed so far, waiting briefly for the client's GOODBYE,
    /// which it sends without waiting for an answer.
    pub(super) async fn closes(&self, expected: usize) -> Vec<Value> {
        for _ in 0..200 {
            let closes = self.log.lock().unwrap().closes.clone();
            if closes.len() >= expected {
                return closes;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        self.log.lock().unwrap().closes.clone()
    }

    pub fn requests(&self) -> Vec<Value> {
        self.log.lock().unwrap().requests.clone()
    }

    pub async fn transport(&self) -> SubcRelayTransport {
        self.transport_with_deadline(None).await
    }

    pub(super) async fn transport_with_deadline(
        &self,
        route_open_deadline: Option<std::time::Duration>,
    ) -> SubcRelayTransport {
        SubcRelayTransport::with_consumer(self.consumer().await, route_open_deadline)
    }

    pub(super) async fn consumer(&self) -> subc_client_rs::SubcConsumer {
        subc_client_rs::SubcConsumer::connect(
            &self.connection_file,
            subc_client_rs::ConsumerOptions {
                call_timeout: std::time::Duration::from_secs(5),
                ..subc_client_rs::ConsumerOptions::default()
            },
        )
        .await
        .expect("the fake daemon accepts the relay's connection")
    }
}

/// Serve one subc connection per accept on the current runtime, as `script`
/// says, recording what arrives.
pub(super) async fn fake_daemon(script: DaemonScript) -> FakeDaemon {
    use subc_protocol::{Flags, Frame, FrameType, ModuleHelloAckBody, Priority, PROTOCOL_VERSION};
    use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let key = vec![0x42; subc_transport::KEY_LEN];
    let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
    let log = Arc::new(Mutex::new(DaemonLog::default()));
    let server_log = Arc::clone(&log);
    let server_key = key.clone();
    tokio::spawn(async move {
        let flags = Flags::new(false, Priority::Passive, false);
        while let Ok((mut stream, _)) = listener.accept().await {
            let key = server_key.clone();
            let log = Arc::clone(&server_log);
            let script = script.clone();
            tokio::spawn(async move {
                let _connection = DaemonConnection(log.clone());
                if subc_transport::authenticate_server(
                    &mut stream,
                    &key,
                    &daemon_id,
                    "0.39.0",
                    std::time::Duration::from_secs(5),
                )
                .await
                .is_err()
                {
                    return;
                }
                let mut next_channel: u16 = 40;
                let mut scoped_channels = Vec::new();
                let mut route_scopes = std::collections::HashMap::new();
                let mut scope_ended = false;
                let mut held = None;
                let mut deferred = Vec::new();
                while let Ok(Some(frame)) = aft_base::subc_fake::read_frame(&mut stream).await {
                    let header = frame.header;
                    let mut replies = Vec::new();
                    match header.ty {
                        FrameType::Hello => replies.push(
                            Frame::build(
                                FrameType::HelloAck,
                                flags,
                                0,
                                0,
                                header.corr,
                                serde_json::to_vec(&ModuleHelloAckBody {
                                    negotiated_ver: PROTOCOL_VERSION,
                                    subc_ops: Vec::new(),
                                    subc_capabilities: Vec::new(),
                                    storage: None,
                                    machine_id: None,
                                })
                                .unwrap(),
                            )
                            .unwrap(),
                        ),
                        FrameType::Request => {
                            let body: Value =
                                serde_json::from_slice(&frame.body).unwrap_or(Value::Null);
                            let respond = |response: Value| {
                                Frame::build_with_version(
                                    header.ver,
                                    FrameType::Response,
                                    header.flags,
                                    header.channel,
                                    header.epoch,
                                    header.corr,
                                    serde_json::to_vec(&response).unwrap(),
                                )
                                .unwrap()
                            };
                            if header.channel != 0 {
                                let scoped = scoped_channels.contains(&header.channel);
                                let seen = {
                                    let mut log = log.lock().unwrap();
                                    log.requests.push(body.clone());
                                    log.request_channels.push(header.channel);
                                    log.requests.len()
                                };
                                let response = script.reply.as_ref().map(|reply| reply(&body,route_scopes.get(&header.channel)))
                                    .unwrap_or_else(|| json!({"repo_binding_generation":1,"result":{"status":"completed"}}));
                                let reply = respond(response);
                                if script.hold_first_request && seen == 1 {
                                    held = Some(reply);
                                    continue;
                                }
                                replies.push(reply);
                                if let Some(first) = held.take() {
                                    deferred.push(first);
                                }
                                if script.end_scope_after_first_write && scoped && !scope_ended {
                                    scope_ended = true;
                                    replies.push(
                                        Frame::build(
                                            FrameType::Push,
                                            flags,
                                            0,
                                            0,
                                            0,
                                            serde_json::to_vec(&json!({
                                                "op": "route.closed",
                                                "module_id": PLEXUS_MODULE_ID,
                                                "reason": "scope_ended",
                                                "drained": false,
                                                "abandoned": 0,
                                                "excluded_subscriptions": 0,
                                            }))
                                            .unwrap(),
                                        )
                                        .unwrap(),
                                    );
                                }
                            } else {
                                match body.get("op").and_then(Value::as_str) {
                                    Some("route.open") => {
                                        log.lock().unwrap().opens.push(body.clone());
                                        let scoped =
                                            body.get("scope").is_some_and(|s| !s.is_null());
                                        let refusal =
                                            script.refuse_scoped_opens.clone().or_else(|| {
                                                scope_ended.then(|| "scope_ended".to_string())
                                            });
                                        match refusal.as_deref() {
                                            Some(code) if scoped => {
                                                if let Some(delay) = script.scoped_refusal_delay {
                                                    let _ =
                                                        tokio::task::spawn_blocking(move || {
                                                            std::thread::sleep(delay)
                                                        })
                                                        .await;
                                                }
                                                replies.push(
                                                    Frame::build(
                                                        FrameType::Error,
                                                        flags,
                                                        0,
                                                        0,
                                                        header.corr,
                                                        serde_json::to_vec(
                                                            &subc_protocol::ErrorBody::new(
                                                                code,
                                                                "refused by the fake daemon",
                                                            ),
                                                        )
                                                        .unwrap(),
                                                    )
                                                    .unwrap(),
                                                );
                                                if script.announce_scoped_refusals {
                                                    replies.push(
                                                        Frame::build(
                                                            FrameType::Push,
                                                            flags,
                                                            0,
                                                            0,
                                                            0,
                                                            serde_json::to_vec(&json!({
                                                                "op": "test.route_open_refused",
                                                                "code": code,
                                                            }))
                                                            .unwrap(),
                                                        )
                                                        .unwrap(),
                                                    );
                                                }
                                            }
                                            _ => {
                                                if scoped {
                                                    if let Some(pause) = &script.scoped_open_pause {
                                                        pause.acquired.notify_one();
                                                        pause.resume.notified().await;
                                                    }
                                                }
                                                next_channel += 1;
                                                if scoped {
                                                    scoped_channels.push(next_channel);
                                                    route_scopes.insert(
                                                        next_channel,
                                                        body["scope"].clone(),
                                                    );
                                                }
                                                replies.push(respond(json!({
                                                    "op": "route.open",
                                                    "route_channel": next_channel,
                                                    "route_epoch": 1,
                                                })));
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        // The client closes a route by sending a GOODBYE frame on the route's own
                        // channel.
                        FrameType::Goodbye if header.channel != 0 => {
                            log.lock()
                                .unwrap()
                                .closes
                                .push(json!({"channel": header.channel}));
                        }
                        _ => {}
                    }
                    for reply in replies {
                        if header.channel != 0 && header.ty == FrameType::Request {
                            if let Some(delay) = script.request_reply_delay {
                                tokio::time::sleep(delay).await;
                            }
                        }
                        if subc_transport::write_frame(&mut stream, &reply)
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    // A held reply goes out a little after the one that
                    // released it, so the second call finishes first.
                    for reply in std::mem::take(&mut deferred) {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        if subc_transport::write_frame(&mut stream, &reply)
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            });
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let connection_file = dir.path().join("connection.json");
    connection_file::write_atomic(
        &connection_file,
        &ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".to_string(),
                port,
            }],
            key,
            daemon_id,
            pid: std::process::id(),
            // Scope tests must reach route.open, which requires daemon 0.39+.
            daemon_ver: "0.39.0".to_string(),
        },
    )
    .unwrap();
    FakeDaemon {
        log,
        connection_file,
        _dir: dir,
    }
}

#[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
pub async fn fake_daemon_with_reply(
    reply: Arc<dyn Fn(&Value, Option<&Value>) -> Value + Send + Sync>,
) -> FakeDaemon {
    fake_daemon(DaemonScript {
        reply: Some(reply),
        ..Default::default()
    })
    .await
}
