use super::*;
use std::sync::{Arc, Condvar};
use subc_protocol::{Flags, Frame, FrameType, Priority};
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};

// Protocol 0.34 clients require numeric daemon versions of at least 0.39.0
// before sending project-free opens. This fixture already handles those opens;
// matching version metadata lets its scope and lease behavior be exercised.
const FAKE_DAEMON_VERSION: &str = "0.39.0";

#[derive(Default)]
pub struct Counts {
    #[doc(hidden)]
    pub opens: usize,
    #[doc(hidden)]
    pub closes: usize,
    #[doc(hidden)]
    pub active: usize,
    #[doc(hidden)]
    pub queries: usize,
    pub(crate) connections: usize,
    /// Connections the fake has accepted and not yet seen close.
    pub(crate) live: usize,
}

pub struct Daemon {
    _directory: tempfile::TempDir,
    #[doc(hidden)]
    pub path: PathBuf,
    #[doc(hidden)]
    pub counts: Arc<(Mutex<Counts>, Condvar)>,
    stop: tokio::sync::oneshot::Sender<()>,
    server: std::thread::JoinHandle<()>,
    pub(super) release: tokio::sync::watch::Sender<bool>,
    release_open: tokio::sync::watch::Sender<bool>,
    _disconnect: tokio::sync::watch::Sender<bool>,
}

impl Daemon {
    pub fn start(query_group: usize, disconnect_first_group: bool) -> Self {
        Self::start_inner(query_group, disconnect_first_group, None)
    }

    pub fn with_held_consent_open(scope: Value) -> Self {
        Self::start_inner(1, false, Some(scope))
    }

    fn start_inner(
        query_group: usize,
        disconnect_first_group: bool,
        held_open_scope: Option<Value>,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connection.json");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let key = vec![0x42; subc_transport::KEY_LEN];
        let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
        connection_file::write_atomic(
            &path,
            &ConnectionInfo {
                schema: SCHEMA_VERSION,
                wire_version: Some(subc_protocol::PROTOCOL_VERSION),
                endpoints: vec![Endpoint {
                    host: "127.0.0.1".into(),
                    port: listener.local_addr().unwrap().port(),
                }],
                key: key.clone(),
                daemon_id,
                pid: std::process::id(),
                daemon_ver: FAKE_DAEMON_VERSION.into(),
            },
        )
        .unwrap();
        let counts = Arc::new((Mutex::new(Counts::default()), Condvar::new()));
        let recorded = counts.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::watch::channel(false);
        let (release_open, open_released) = tokio::sync::watch::channel(false);
        let (disconnect, disconnected) = tokio::sync::watch::channel(false);
        let server = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let mut tasks = tokio::task::JoinSet::new();
                tokio::pin!(stopped);
                loop {
                    tokio::select! {
                        _ = &mut stopped => break,
                        accepted = listener.accept() => {
                            let (mut stream, _) = accepted.unwrap();
                            let key = key.clone();
                            let counts = recorded.clone();
                            let mut released = released.clone();
                            let mut open_released = open_released.clone();
                            let mut disconnected = disconnected.clone();
                            let held_open_scope = held_open_scope.clone();
                            tasks.spawn(async move {
                                subc_transport::authenticate_server(&mut stream, &key, &daemon_id, FAKE_DAEMON_VERSION, Duration::from_secs(2)).await.unwrap();
                                let connection = {
                                    let mut count = counts.0.lock().unwrap();
                                    count.connections += 1;
                                    count.live += 1;
                                    count.connections
                                };
                                let mut active = 0;
                                let mut pending = Vec::new();
                                let mut held = Vec::new();
                                let mut held_opens = Vec::new();
                                let mut generation = 0;
                                loop {
                                    let request = tokio::select! {
                                        Ok(()) = disconnected.changed(), if connection == 1 => break,
                                        request = aft_base::subc_fake::read_frame(&mut stream) => match request {
                                            Ok(Some(request)) => request,
                                            _ => break,
                                        },
                                        Ok(()) = released.changed() => {
                                            for (request, value) in held.drain(..) {
                                                respond(&mut stream, &request, value, false).await;
                                            }
                                            continue;
                                        },
                                        Ok(()) = open_released.changed() => {
                                            for (request, value) in held_opens.drain(..) {
                                                respond(&mut stream, &request, value, false).await;
                                            }
                                            continue;
                                        }
                                    };
                                    // Slow-query tests withhold application replies, not transport
                                    // liveness. Ignoring probes makes the SDK retire a healthy route.
                                    if request.header.ty == FrameType::Ping {
                                        let pong = Frame::build(FrameType::Pong, Flags::new(false, Priority::Interactive, false), 0, 0, request.header.corr, Vec::new()).unwrap();
                                        subc_transport::write_frame(&mut stream, &pong).await.unwrap();
                                        continue;
                                    }
                                    if request.header.ty == FrameType::Goodbye {
                                        active -= 1;
                                        let mut count = counts.0.lock().unwrap();
                                        count.active -= 1;
                                        count.closes += 1;
                                        counts.1.notify_all();
                                        continue;
                                    }
                                    if request.header.ty != FrameType::Request { continue; }
                                    let value: Value = serde_json::from_slice(&request.body).unwrap();
                                    let reply = match value["op"].as_str() {
                                        Some("catalog.list") => json!({"op":"catalog.list","generation":1,"modules":[{
                                            "module_id":"synapse","module_version":"1","roles":[{"role":"management_surface",
                                            "operations":[{"name":"models.list","kind":"query"},{"name":"embed.query","kind":"query"},{"name":"embed.batch","kind":"query"},{"name":"rerank.score","kind":"query"}],
                                            "config_schema":{"type":"object"},"observability":[],"identity_scope":[]}],"control_ops":[]}],"subc_ops":["catalog.list","route.open"]}),
                                        Some("route.open") => {
                                            {
                                                let mut count = counts.0.lock().unwrap();
                                                count.opens += 1;
                                                generation = count.opens;
                                                count.active += 1;
                                            }
                                            active += 1;
                                            counts.1.notify_all();
                                            let reply = json!({"op":"route.open","route_channel":6 + generation,"route_epoch":1});
                                            if held_open_scope.as_ref() == Some(&value["scope"]) && !*open_released.borrow() {
                                                held_opens.push((request, reply));
                                                continue;
                                            }
                                            reply
                                        },
                                        _ if value["method"] == MODELS_LIST_OPERATION => json!({"models":[{"model":pause_test_support::MODEL,"fingerprint":"pinned","table_epoch":1,"dims":2,"recommended_batch":{"rows":100,"token_budget":8192}}]}),
                                        _ if value["method"] == QUERY_OPERATION || value["method"] == "rerank.score" => {
                                            counts.0.lock().unwrap().queries += 1;
                                            counts.1.notify_all();
                                            match value["params"]["text"].as_str() {
                                                Some("timeout") => continue,
                                                Some("held") if !*released.borrow() => {
                                                    held.push((request, query_reply(&value)));
                                                    continue;
                                                },
                                                Some("module-error") => {
                                                    respond(&mut stream, &request, json!({"code":"test_refused","message":"request rejected"}), true).await;
                                                    continue;
                                                },
                                                Some("malformed") => {
                                                    let frame = Frame::build(FrameType::Response, Flags::new(false, Priority::Interactive, false), request.header.channel, request.header.epoch, request.header.corr, b"{".to_vec()).unwrap();
                                                    subc_transport::write_frame(&mut stream, &frame).await.unwrap();
                                                    continue;
                                                },
                                                Some("lane-paused") => {
                                                    respond(&mut stream, &request, pause_test_support::pause_reply(json!(1_000)), false).await;
                                                    continue;
                                                },
                                                Some("application-error") => {
                                                    respond(&mut stream, &request, json!({"error":{"code":"test_rejected","message":"application rejected"}}), false).await;
                                                    continue;
                                                },
                                                _ => {}
                                            }
                                            pending.push((request, value));
                                            if pending.len() < query_group { continue; }
                                            // The real consumer enforces its 1024-request route window.
                                            // Holding these replies proves requests overlap on the wire.
                                            if disconnect_first_group && generation == 1 {
                                                // Keep every caller pending until the test drops the socket.
                                                continue;
                                            }
                                            for (request, value) in pending.drain(..) {
                                                respond(&mut stream, &request, query_reply(&value), false).await;
                                            }
                                            continue;
                                        },
                                        _ if value["method"] == "consent.test_timeout" => {
                                            counts.0.lock().unwrap().queries += 1;
                                            counts.1.notify_all();
                                            continue;
                                        },
                                        _ if value["method"] == "consent.test_held" => {
                                            counts.0.lock().unwrap().queries += 1;
                                            counts.1.notify_all();
                                            if !*released.borrow() {
                                                held.push((request, json!({"result":{"ok":true}})));
                                                continue;
                                            }
                                            json!({"result":{"ok":true}})
                                        },
                                        _ if value["method"] == "consent.test_error" => {
                                            respond(&mut stream, &request, json!({"code":"test_refused","message":"refused"}), true).await;
                                            continue;
                                        },
                                        _ => json!({"result":{"ok":true}}),
                                    };
                                    respond(&mut stream, &request, reply, false).await;
                                }
                                {
                                    let mut count = counts.0.lock().unwrap();
                                    count.active -= active;
                                    count.live -= 1;
                                }
                                counts.1.notify_all();
                            });
                        }
                    }
                }
                tasks.abort_all();
            });
        });
        Self {
            _directory: directory,
            path,
            counts,
            stop,
            server,
            release,
            release_open,
            _disconnect: disconnect,
        }
    }

    pub fn release_consent_open(&self) {
        self.release_open.send(true).unwrap();
    }

    pub fn release_replies(&self) {
        self.release.send(true).unwrap();
    }

    #[cfg(test)]
    pub(super) fn disconnect_first_connection(&self) {
        self._disconnect.send(true).unwrap();
    }

    #[cfg(test)]
    pub(super) fn config(&self, root: usize) -> SemanticBackendConfig {
        SemanticBackendConfig {
            backend: crate::config::SemanticBackend::Synapse,
            model: pause_test_support::MODEL.into(),
            subc_connection_file: Some(self.path.clone()),
            route_project_root: Some(self._directory.path().join(format!("root-{root}"))),
            timeout_ms: 5_000,
            query_instruction: "off".into(),
            ..Default::default()
        }
    }

    pub fn wait_closed(&self) {
        let (count, _) = self
            .counts
            .1
            .wait_timeout_while(
                self.counts.0.lock().unwrap(),
                Duration::from_secs(2),
                |count| count.active != 0,
            )
            .unwrap();
        assert_eq!(count.active, 0, "completed clients left routes open");
    }

    /// Wait until the fake holds exactly `live` open connections.
    #[cfg(test)]
    pub(super) fn wait_live(&self, live: usize) {
        let (count, _) = self
            .counts
            .1
            .wait_timeout_while(
                self.counts.0.lock().unwrap(),
                Duration::from_secs(5),
                |count| count.live != live,
            )
            .unwrap();
        assert_eq!(count.live, live, "unexpected open daemon connections");
    }

    #[cfg(test)]
    pub(super) fn wait_queries(&self, queries: usize) {
        self.wait_queries_with_timeout(queries, Duration::from_secs(2));
    }

    #[cfg(test)]
    pub(super) fn wait_queries_with_timeout(&self, queries: usize, timeout: Duration) {
        let (counts, _) = self
            .counts
            .1
            .wait_timeout_while(self.counts.0.lock().unwrap(), timeout, |counts| {
                counts.queries < queries
            })
            .unwrap();
        assert!(
            counts.queries >= queries,
            "expected {queries} requests on the wire, received {}",
            counts.queries
        );
    }

    pub fn finish(self) {
        let _ = self.stop.send(());
        self.server.join().unwrap();
    }
}

fn query_reply(value: &Value) -> Value {
    json!({"fingerprint":"pinned","table_epoch":1,"vectors":[{"id":value["params"]["id"],"content_sha256":value["params"]["content_sha256"],"vector":[1.0,0.0]}]})
}

async fn respond(stream: &mut tokio::net::TcpStream, request: &Frame, value: Value, error: bool) {
    let frame = Frame::build_with_version(
        request.header.ver,
        if error {
            FrameType::Error
        } else {
            FrameType::Response
        },
        Flags::new(false, Priority::Interactive, false),
        request.header.channel,
        request.header.epoch,
        request.header.corr,
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    let _ = subc_transport::write_frame(stream, &frame).await;
}
