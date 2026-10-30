//! Synapse reranking discovers and fixes its model pin when connecting, off the
//! search path. `fingerprint()` is a no-I/O read and never changes for a backend
//! instance, so paging can safely memoize outcomes under that identity.

use parking_lot::Mutex;
use std::time::Instant;

use serde_json::{json, Value};

use crate::commands::semantic_search::rerank::{
    RerankBackend, RerankDoc, RerankError, RerankFingerprint,
};
use crate::config::SemanticBackendConfig;
use crate::synapse_embed::{SynapseEmbeddingError, SynapseRerankTransport};

pub(crate) struct SynapseReranker {
    model: String,
    required_fingerprint: String,
    max_queue_ms: u64,
    transport: Mutex<SynapseRerankTransport>,
}

impl SynapseReranker {
    /// The embedding configuration supplies only the daemon connection and route identity.
    /// The rerank model and its pin are independent of the embedding model.
    pub(crate) fn connect(
        config: &SemanticBackendConfig,
        model: String,
        required_fingerprint: Option<String>,
        max_queue_ms: u64,
        under_subc: bool,
        deadline: Instant,
    ) -> Result<Self, RerankError> {
        if !under_subc {
            return Err(RerankError::Unavailable(
                "Synapse reranking is daemon-only; run AFT under subc with Synapse registered"
                    .into(),
            ));
        }
        if model.trim().is_empty()
            || required_fingerprint
                .as_ref()
                .is_some_and(|pin| pin.trim().is_empty())
        {
            return Err(RerankError::Refused(
                "Synapse reranking requires a model and a non-empty explicit fingerprint when provided".into(),
            ));
        }
        let mut transport = SynapseRerankTransport::new(config).map_err(transport_error)?;
        let pin = match required_fingerprint {
            Some(pin) => pin,
            None => {
                let response = transport
                    .call("models.list", json!({}), deadline)
                    .map_err(transport_error)?;
                discover_pin(&response, &model)?
            }
        };
        Ok(Self {
            model,
            required_fingerprint: pin,
            max_queue_ms,
            transport: Mutex::new(transport),
        })
    }
}

fn discover_pin(response: &Value, model: &str) -> Result<String, RerankError> {
    let envelope = response.get("result").unwrap_or(response);
    let entries = envelope
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| RerankError::Failed("Synapse models.list omitted models".into()))?;
    let entry = entries
        .iter()
        .find(|entry| {
            entry
                .get("model_id")
                .or_else(|| entry.get("model"))
                .and_then(Value::as_str)
                == Some(model)
        })
        .ok_or_else(|| {
            RerankError::Unavailable(format!("Synapse does not serve reranker {model}"))
        })?;
    entry
        .get("fingerprint")
        .and_then(Value::as_str)
        .or_else(|| {
            entry
                .get("fingerprints")
                .and_then(Value::as_array)
                .and_then(|values| values.first())
                .and_then(Value::as_str)
        })
        .filter(|pin| !pin.is_empty())
        .map(str::to_string)
        .ok_or_else(|| RerankError::Failed("Synapse reranker has no fingerprint".into()))
}

impl RerankBackend for SynapseReranker {
    fn fingerprint(&self) -> RerankFingerprint {
        RerankFingerprint {
            backend: "synapse",
            model: self.model.clone(),
            revision: self.required_fingerprint.clone(),
        }
    }
    fn max_batch(&self) -> usize {
        20
    }
    fn score(
        &self,
        query: &str,
        docs: &[RerankDoc<'_>],
        deadline: Instant,
    ) -> Result<Vec<f32>, RerankError> {
        if docs.len() > self.max_batch() {
            return Err(RerankError::Refused(
                "Synapse interactive reranking accepts at most 20 candidates".into(),
            ));
        }
        if docs.is_empty() {
            return Ok(Vec::new());
        }
        // Never wait behind another request beyond this call's deadline.
        let mut transport = self
            .transport
            .try_lock_until(deadline)
            .ok_or(RerankError::Timeout)?;
        let pin = &self.required_fingerprint;
        let result = score_via(
            &self.model,
            pin,
            self.max_queue_ms,
            query,
            docs,
            deadline,
            |params| {
                transport
                    .call("rerank.score", params, deadline)
                    .map_err(transport_error)
            },
        );
        if matches!(&result, Err(RerankError::Refused(message)) if message.contains("substitution_rejected"))
        {
            if let Ok(response) = transport.call("models.list", json!({}), deadline) {
                if let Ok(current) = discover_pin(&response, &self.model) {
                    return Err(RerankError::Refused(format!("Synapse rerank fingerprint changed: pinned {pin}, served {current}; rebuild the backend to re-pin")));
                }
            }
        }
        result
    }
}

fn transport_error(error: SynapseEmbeddingError) -> RerankError {
    match error {
        SynapseEmbeddingError::Timeout(_) => RerankError::Timeout,
        SynapseEmbeddingError::InvalidEnvelope(message) => RerankError::Failed(message),
        other => RerankError::Unavailable(other.to_string()),
    }
}

fn score_via(
    model: &str,
    pin: &str,
    max_queue_ms: u64,
    query: &str,
    docs: &[RerankDoc<'_>],
    deadline: Instant,
    call: impl FnOnce(Value) -> Result<Value, RerankError>,
) -> Result<Vec<f32>, RerankError> {
    if docs.len() > 20 {
        return Err(RerankError::Refused(
            "Synapse interactive reranking accepts at most 20 candidates".into(),
        ));
    }
    if docs.is_empty() {
        return Ok(Vec::new());
    }
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or(RerankError::Timeout)?;
    let deadline_ms = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
    if deadline_ms == 0 {
        return Err(RerankError::Timeout);
    }
    let response = call(
        json!({"model":model, "query":query, "candidates":docs.iter().map(|doc| doc.text).collect::<Vec<_>>(), "required_fingerprint":pin, "allow_equivalent":false, "accept_declared":false, "max_queue_ms":max_queue_ms.min(deadline_ms), "deadline_ms":deadline_ms}),
    )?;
    parse_response(&response, pin, docs.len())
}

fn parse_response(response: &Value, pin: &str, count: usize) -> Result<Vec<f32>, RerankError> {
    let envelope = response.get("result").unwrap_or(response);
    if let Some(error) = envelope.get("error") {
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(match code {
            "deadline_exceeded" | "timeout" => RerankError::Timeout,
            "model_not_found" | "model_unavailable" | "not_certified" => {
                RerankError::Unavailable(format!("Synapse rerank {code}"))
            }
            _ => RerankError::Refused(format!("Synapse rerank {code}")),
        });
    }
    if envelope.get("fingerprint").and_then(Value::as_str) != Some(pin) {
        return Err(RerankError::Refused(format!(
            "Synapse rerank fingerprint changed: pinned {pin}, served {}",
            envelope
                .get("fingerprint")
                .and_then(Value::as_str)
                .unwrap_or("missing")
        )));
    }
    let scores = envelope
        .get("scores")
        .and_then(Value::as_array)
        .filter(|scores| scores.len() == count)
        .ok_or_else(|| {
            RerankError::Failed("Synapse rerank response omitted candidate scores".into())
        })?;
    // Synapse serves raw cross-encoder logits, not normalized relevance probabilities.
    scores
        .iter()
        .map(|score| {
            score
                .as_f64()
                .map(|score| score as f32)
                .filter(|score| score.is_finite())
                .ok_or_else(|| {
                    RerankError::Failed("Synapse rerank returned a non-finite score".into())
                })
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Duration;

    fn connect(
        config: &SemanticBackendConfig,
        model: String,
        pin: Option<String>,
        queue_ms: u64,
        under_subc: bool,
    ) -> Result<SynapseReranker, RerankError> {
        SynapseReranker::connect(
            config,
            model,
            pin,
            queue_ms,
            under_subc,
            Instant::now() + Duration::from_secs(2),
        )
    }

    pub(crate) fn config(connection_file: std::path::PathBuf) -> SemanticBackendConfig {
        SemanticBackendConfig {
            local_cache_dir: None,
            backend: crate::config::SemanticBackend::Synapse,
            model: "embedding".into(),
            base_url: None,
            api_key_env: None,
            timeout_ms: 1500,
            query_timeout_ms: 1500,
            query_instruction: "off".into(),
            max_batch_size: 20,
            max_input_tokens: None,
            max_files: 100,
            subc_connection_file: Some(connection_file),
            route_project_root: None,
            route_harness: None,
        }
    }

    pub(crate) fn fake_daemon(
        responses: Vec<Option<Value>>,
        registered: bool,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::thread::JoinHandle<Vec<Value>>,
    ) {
        fake_daemon_gated(responses, registered, None)
    }

    pub(crate) type ResponseGate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

    fn connection_fixture() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::net::TcpListener,
        Vec<u8>,
        [u8; subc_transport::DAEMON_ID_LEN],
    ) {
        use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};
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
                daemon_ver: "synapse-test".into(),
            },
        )
        .unwrap();
        (directory, path, listener, key, daemon_id)
    }

    pub(crate) fn fake_daemon_gated(
        responses: Vec<Option<Value>>,
        registered: bool,
        gate: Option<ResponseGate>,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::thread::JoinHandle<Vec<Value>>,
    ) {
        let (directory, path, listener, key, daemon_id) = connection_fixture();
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                tokio::time::timeout(Duration::from_secs(30), async move {
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                    let (mut stream, _) = listener.accept().await.unwrap();
                    subc_transport::authenticate_server(&mut stream, &key, &daemon_id, "synapse-test", Duration::from_secs(30)).await.unwrap();
                    let catalog = subc_transport::read_frame(&mut stream).await.unwrap().unwrap();
                    let modules = if registered { json!([{"module_id":"synapse","module_version":"1","roles":[{"role":"management_surface","operations":[{"name":"models.list","kind":"query"},{"name":"rerank.score","kind":"query"}],"config_schema":{"type":"object"},"observability":[],"identity_scope":[]}],"control_ops":[]}]) } else { json!([]) };
                    respond(&mut stream, &catalog, json!({"op":"catalog.list","generation":1,"modules":modules,"subc_ops":["catalog.list","route.open"]})).await;
                    if !registered { return Vec::new(); }
                    let route = subc_transport::read_frame(&mut stream).await.unwrap().unwrap();
                    let route_body: Value = serde_json::from_slice(&route.body).unwrap();
                    assert_eq!(route_body["target"], json!({"kind":"management_surface","module_id":"synapse"}));
                    respond(&mut stream, &route, json!({"op":"route.open","route_channel":7,"route_epoch":1})).await;
                    let mut requests = Vec::new();
                    let mut gate = gate;
                    for response in responses {
                        let request = subc_transport::read_frame(&mut stream).await.unwrap().unwrap();
                        requests.push(serde_json::from_slice(&request.body).unwrap());
                        if let Some((entered, release)) = gate.take() {
                            entered.send(()).unwrap();
                            release.recv_timeout(Duration::from_secs(2)).unwrap();
                        }
                        if let Some(response) = response { respond(&mut stream, &request, response).await; }
                        else {
                            // Withhold the reply until the client has timed out, then detect any retry.
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            if let Ok(frame) = tokio::time::timeout(Duration::from_millis(50), subc_transport::read_frame(&mut stream)).await {
                                assert!(frame.unwrap().is_none(), "unexpected retry on route");
                            }
                            assert!(tokio::time::timeout(Duration::from_millis(50), listener.accept()).await.is_err());
                        }
                    }
                    requests
                }).await.expect("fake Synapse watchdog")
            })
        });
        (directory, path, server)
    }

    async fn respond(
        stream: &mut tokio::net::TcpStream,
        request: &subc_protocol::Frame,
        value: Value,
    ) {
        use subc_protocol::{Flags, Frame, FrameType, Priority};
        let frame = Frame::build_with_version(
            request.header.ver,
            FrameType::Response,
            Flags::new(false, Priority::Interactive, false),
            request.header.channel,
            request.header.epoch,
            request.header.corr,
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        subc_transport::write_frame(stream, &frame).await.unwrap();
    }

    #[test]
    fn synapse_explicit_pin_and_fingerprint_make_zero_transport_calls() {
        let (_directory, path, listener, _, _) = connection_fixture();
        let backend = connect(
            &config(path),
            "reranker".into(),
            Some("fp".into()),
            100,
            true,
        )
        .unwrap();
        assert_eq!(backend.fingerprint().revision, "fp");
        assert_eq!(backend.fingerprint().revision, "fp");
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn synapse_connect_refuses_failed_discovery_and_bounds_its_deadline() {
        let (_directory, path, server) =
            fake_daemon(vec![Some(json!({"result":{"models":[]}}))], true);
        assert!(matches!(
            connect(&config(path), "reranker".into(), None, 100, true),
            Err(RerankError::Unavailable(_))
        ));
        assert_eq!(server.join().unwrap().len(), 1);
        let (_directory, path, server) = fake_daemon(vec![None], true);
        assert!(matches!(
            SynapseReranker::connect(
                &config(path),
                "reranker".into(),
                None,
                100,
                true,
                Instant::now() + Duration::from_millis(200)
            ),
            Err(RerankError::Timeout)
        ));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    fn spawn_score(
        backend: std::sync::Arc<SynapseReranker>,
    ) -> std::thread::JoinHandle<Result<Vec<f32>, RerankError>> {
        std::thread::spawn(move || {
            backend.score(
                "q",
                &[RerankDoc { text: "a" }],
                Instant::now() + Duration::from_secs(2),
            )
        })
    }

    #[test]
    fn synapse_concurrent_scores_wait_for_transport_and_both_succeed() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (_directory, path, server) = fake_daemon_gated(
            vec![
                Some(json!({"result":{"fingerprint":"fp","scores":[2.0]}})),
                Some(json!({"result":{"fingerprint":"fp","scores":[2.0]}})),
            ],
            true,
            Some((entered_tx, release_rx)),
        );
        let backend = std::sync::Arc::new(
            connect(
                &config(path),
                "reranker".into(),
                Some("fp".into()),
                100,
                true,
            )
            .unwrap(),
        );
        let first = spawn_score(backend.clone());
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // Identity remains a cheap read while scoring is waiting on the daemon.
        assert_eq!(backend.fingerprint().revision, "fp");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = backend.score(
                "q",
                &[RerankDoc { text: "a" }],
                Instant::now() + Duration::from_secs(2),
            );
            done_tx.send(result).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let waiting = matches!(
            done_rx.recv_timeout(Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        );
        release_tx.send(()).unwrap();
        assert!(
            waiting,
            "second score must wait for the transport rather than skip reranking"
        );
        assert_eq!(first.join().unwrap().unwrap(), vec![2.0]);
        assert_eq!(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap(),
            vec![2.0]
        );
        second.join().unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "models.list")
                .count(),
            0
        );
    }

    #[test]
    fn synapse_concurrent_score_times_out_at_its_lock_deadline() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (_directory, path, server) = fake_daemon_gated(
            vec![Some(json!({"result":{"fingerprint":"fp","scores":[2.0]}}))],
            true,
            Some((entered_tx, release_rx)),
        );
        let backend = std::sync::Arc::new(
            connect(
                &config(path),
                "reranker".into(),
                Some("fp".into()),
                100,
                true,
            )
            .unwrap(),
        );
        let first = spawn_score(backend.clone());
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        let deadline = started + Duration::from_millis(50);
        let result = backend.score("q", &[RerankDoc { text: "a" }], deadline);
        let finished = Instant::now();
        release_tx.send(()).unwrap();
        assert!(matches!(result, Err(RerankError::Timeout)));
        assert!(
            finished >= deadline,
            "lock timeout must not fail before the caller's deadline"
        );
        assert!(
            finished < started + Duration::from_secs(1),
            "lock wait must stay bounded"
        );
        assert_eq!(first.join().unwrap().unwrap(), vec![2.0]);
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn synapse_transport_discovers_once_and_keeps_pin_after_model_change() {
        let models =
            |pin| json!({"result":{"models":[{"model_id":"reranker","fingerprints":[pin]}]}});
        let (_directory, path, server) = fake_daemon(
            vec![
                Some(models("fp-old")),
                Some(json!({"result":{"fingerprint":"fp-old","scores":[2.0]}})),
                Some(json!({"result":{"error":{"code":"substitution_rejected"}}})),
                Some(models("fp-new")),
            ],
            true,
        );
        let mut config = config(path);
        config.timeout_ms = 30_000;
        config.query_timeout_ms = 30_000;
        let backend = SynapseReranker::connect(
            &config,
            "reranker".into(),
            None,
            100,
            true,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(backend.fingerprint().revision, "fp-old");
        assert_eq!(
            backend
                .score(
                    "q",
                    &[RerankDoc { text: "a" }],
                    Instant::now() + Duration::from_secs(30)
                )
                .unwrap(),
            vec![2.0]
        );
        let error = backend
            .score(
                "q",
                &[RerankDoc { text: "a" }],
                Instant::now() + Duration::from_secs(30),
            )
            .unwrap_err();
        assert!(
            matches!(error, RerankError::Refused(message) if message.contains("fp-old") && message.contains("fp-new"))
        );
        assert_eq!(backend.fingerprint().revision, "fp-old");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0]["method"], "models.list");
        assert_eq!(requests[1]["params"]["required_fingerprint"], "fp-old");
        assert_eq!(requests[2]["params"]["required_fingerprint"], "fp-old");
    }

    #[test]
    fn synapse_backend_refuses_bulk_before_scoring() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let backend = connect(
            &config(file.path().into()),
            "reranker".into(),
            Some("fp".into()),
            100,
            true,
        )
        .unwrap();
        let docs: Vec<_> = (0..21).map(|_| RerankDoc { text: "a" }).collect();
        assert!(matches!(
            backend.score("q", &docs, Instant::now() + Duration::from_secs(1)),
            Err(RerankError::Refused(_))
        ));
        assert_eq!(
            backend.score("q", &[], Instant::now()).unwrap(),
            Vec::<f32>::new()
        );
    }

    #[test]
    fn synapse_transport_times_out_without_retry() {
        let (_directory, path, server) = fake_daemon(vec![None], true);
        let backend = connect(
            &config(path),
            "reranker".into(),
            Some("fp".into()),
            100,
            true,
        )
        .unwrap();
        assert!(matches!(
            backend.score(
                "q",
                &[RerankDoc { text: "a" }],
                Instant::now() + Duration::from_millis(200)
            ),
            Err(RerankError::Timeout)
        ));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn synapse_daemon_only_and_registration_status_are_clear() {
        assert!(
            matches!(connect(&config("/missing".into()), "r".into(), None, 100, false), Err(RerankError::Unavailable(message)) if message.contains("daemon-only"))
        );
        let (_directory, path, server) = fake_daemon(Vec::new(), false);
        let backend = connect(
            &config(path),
            "reranker".into(),
            Some("fp".into()),
            100,
            true,
        )
        .unwrap();
        assert!(
            matches!(backend.score("q", &[RerankDoc{text:"a"}], Instant::now()+Duration::from_secs(1)), Err(RerankError::Unavailable(message)) if message.contains("not registered"))
        );
        assert!(server.join().unwrap().is_empty());
    }

    #[test]
    fn synapse_fake_route_pins_identity_and_bounds_admission() {
        let scores = score_via(
            "reranker",
            "fp-a",
            5000,
            "q",
            &[RerankDoc { text: "a" }, RerankDoc { text: "b" }],
            Instant::now() + Duration::from_secs(1),
            |params| {
                assert_eq!(params["model"], "reranker");
                assert_eq!(params["query"], "q");
                assert_eq!(params["candidates"], json!(["a", "b"]));
                assert_eq!(params["required_fingerprint"], "fp-a");
                assert_eq!(params["allow_equivalent"], false);
                assert!(params["deadline_ms"].as_u64().unwrap() <= 1000);
                assert_eq!(params["max_queue_ms"], params["deadline_ms"]);
                Ok(json!({"result":{"fingerprint":"fp-a","scores":[-2.0,3.0]}}))
            },
        )
        .unwrap();
        assert_eq!(scores, vec![-2.0, 3.0]);
    }
    #[test]
    fn synapse_rejects_bulk_and_expired_requests_without_routing() {
        let docs: Vec<_> = (0..21).map(|_| RerankDoc { text: "a" }).collect();
        assert!(matches!(
            score_via(
                "m",
                "fp",
                100,
                "q",
                &docs,
                Instant::now() + Duration::from_secs(1),
                |_| panic!("must not route bulk")
            ),
            Err(RerankError::Refused(_))
        ));
        assert!(matches!(
            score_via("m", "fp", 100, "q", &docs[..1], Instant::now(), |_| panic!(
                "must not route expired call"
            )),
            Err(RerankError::Timeout)
        ));
    }
    #[test]
    fn synapse_rejects_changed_fingerprint_and_malformed_scores() {
        assert!(matches!(
            parse_response(&json!({"fingerprint":"foreign","scores":[1.0]}), "fp", 1),
            Err(RerankError::Refused(_))
        ));
        for scores in [json!([]), json!([1e100]), json!(["NaN"])] {
            assert!(matches!(
                parse_response(&json!({"fingerprint":"fp","scores":scores}), "fp", 1),
                Err(RerankError::Failed(_))
            ));
        }
    }
    #[test]
    fn synapse_error_envelopes_map_to_backend_errors() {
        assert!(matches!(
            parse_response(&json!({"error":{"code":"deadline_exceeded"}}), "fp", 1),
            Err(RerankError::Timeout)
        ));
        assert!(matches!(
            parse_response(&json!({"error":{"code":"model_not_found"}}), "fp", 1),
            Err(RerankError::Unavailable(_))
        ));
        assert!(matches!(
            parse_response(&json!({"error":{"code":"substitution_rejected"}}), "fp", 1),
            Err(RerankError::Refused(_))
        ));
    }
}
