use std::time::Instant;

use serde_json::{json, Value};

use crate::commands::semantic_search::rerank::{
    RerankBackend, RerankDoc, RerankError, RerankFingerprint,
};

/// `tei+http://` and `tei+https://` select the TEI wire format without a new config key.
pub(crate) struct RemoteReranker {
    endpoint: String,
    model: String,
    api_key_env: Option<String>,
    max_batch: usize,
    tei: bool,
}

impl RemoteReranker {
    pub(crate) fn new(
        endpoint: &str,
        model: String,
        api_key_env: Option<String>,
        max_batch: Option<usize>,
    ) -> Result<Self, RerankError> {
        let (endpoint, tei) = endpoint
            .strip_prefix("tei+")
            .map_or((endpoint, false), |url| (url, true));
        let url = url::Url::parse(endpoint)
            .map_err(|_| RerankError::Refused("invalid rerank endpoint".into()))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(RerankError::Refused("rerank endpoint must be an HTTP(S) base URL without credentials, query or fragment".into()));
        }
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').into(),
            model,
            api_key_env,
            max_batch: max_batch.unwrap_or(20).max(1),
            tei,
        })
    }
}

impl RerankBackend for RemoteReranker {
    fn fingerprint(&self) -> RerankFingerprint {
        RerankFingerprint {
            backend: "remote",
            model: self.model.clone(),
            revision: format!(
                "{}:{}",
                if self.tei { "tei" } else { "rerank" },
                self.endpoint
            ),
        }
    }
    fn max_batch(&self) -> usize {
        self.max_batch
    }
    fn score(
        &self,
        query: &str,
        docs: &[RerankDoc<'_>],
        deadline: Instant,
    ) -> Result<Vec<f32>, RerankError> {
        if docs.len() > self.max_batch {
            return Err(RerankError::Refused(
                "rerank batch exceeds configured maximum".into(),
            ));
        }
        if docs.is_empty() {
            return Ok(Vec::new());
        }
        deadline
            .checked_duration_since(Instant::now())
            .filter(|time| !time.is_zero())
            .ok_or(RerankError::Timeout)?;
        let client = crate::platform_tls::blocking_client()
            .map_err(|_| RerankError::Unavailable("cannot initialize rerank HTTP client".into()))?;
        let documents: Vec<_> = docs.iter().map(|doc| doc.text).collect();
        let body = if self.tei {
            json!({"query": query, "texts": documents})
        } else {
            json!({"model": self.model, "query": query, "documents": documents, "top_n": docs.len()})
        };
        let mut request = client.post(format!("{}/rerank", self.endpoint)).json(&body);
        if let Some(name) = &self.api_key_env {
            let key = std::env::var(name)
                .ok()
                .filter(|key| !key.is_empty())
                .ok_or_else(|| {
                    RerankError::Unavailable("rerank API key environment variable is unset".into())
                })?;
            request = request.bearer_auth(key);
        }
        // Recompute after client construction so setup time consumes the caller's budget.
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|time| !time.is_zero())
            .ok_or(RerankError::Timeout)?;
        let response = request.timeout(remaining).send().map_err(http_error)?;
        let status = response.status();
        if !status.is_success() {
            return Err(if status.is_client_error() {
                RerankError::Refused(format!("rerank HTTP {}", status.as_u16()))
            } else {
                RerankError::Unavailable(format!("rerank HTTP {}", status.as_u16()))
            });
        }
        let value: Value = response.json().map_err(http_error)?;
        parse_scores(&value, docs.len(), self.tei)
    }
}

fn http_error(error: reqwest::Error) -> RerankError {
    if error.is_timeout() {
        RerankError::Timeout
    } else if error.is_connect() {
        RerankError::Unavailable("rerank connection failed".into())
    } else {
        RerankError::Failed("invalid rerank HTTP response".into())
    }
}

fn parse_scores(value: &Value, count: usize, tei: bool) -> Result<Vec<f32>, RerankError> {
    let fail = || {
        RerankError::Failed(
            "rerank response must contain one finite score in [0,1] per document index".into(),
        )
    };
    let rows = (if tei {
        Some(value)
    } else {
        value.get("results")
    })
    .and_then(Value::as_array)
    .ok_or_else(fail)?;
    let mut scores = vec![None; count];
    for row in rows {
        let index = row
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .filter(|index| *index < count)
            .ok_or_else(fail)?;
        let score = row
            .get(if tei { "score" } else { "relevance_score" })
            .and_then(Value::as_f64)
            .filter(|score| score.is_finite() && (0.0..=1.0).contains(score))
            .ok_or_else(fail)? as f32;
        if scores[index].replace(score).is_some() {
            return Err(fail());
        }
    }
    scores
        .into_iter()
        .map(|score| score.ok_or_else(fail))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    pub(crate) fn serve(
        body: &'static str,
        status: u16,
        delay: Duration,
    ) -> (String, std::thread::JoinHandle<Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            let body_start;
            let length;
            loop {
                let mut buf = [0; 1024];
                let read = stream.read(&mut buf).unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&buf[..read]);
                if let Some(start) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    body_start = start + 4;
                    let headers = String::from_utf8_lossy(&bytes[..start]);
                    assert!(headers.starts_with("POST /rerank HTTP/1.1"));
                    length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(|value| value.parse::<usize>().unwrap())
                        })
                        .unwrap();
                    break;
                }
            }
            while bytes.len() < body_start + length {
                let mut buf = [0; 1024];
                let read = stream.read(&mut buf).unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&buf[..read]);
            }
            let request = serde_json::from_slice(&bytes[body_start..body_start + length]).unwrap();
            std::thread::sleep(delay);
            let _ = write!(stream, "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            request
        });
        (endpoint, thread)
    }

    #[test]
    fn remote_http_reorders_indices_and_sends_full_batch() {
        let (endpoint, server) = serve(
            r#"{"results":[{"index":1,"relevance_score":0.8},{"index":0,"relevance_score":0.2}]}"#,
            200,
            Duration::ZERO,
        );
        let backend = RemoteReranker::new(&endpoint, "model".into(), None, Some(2)).unwrap();
        let scores = backend
            .score(
                "query",
                &[RerankDoc { text: "a" }, RerankDoc { text: "b" }],
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
        assert_eq!(scores, vec![0.2, 0.8]);
        assert_eq!(
            server.join().unwrap(),
            json!({"model":"model","query":"query","documents":["a","b"],"top_n":2})
        );
    }
    #[test]
    fn remote_tei_http_adapter() {
        let (endpoint, server) = serve(r#"[{"index":0,"score":0.6}]"#, 200, Duration::ZERO);
        let backend =
            RemoteReranker::new(&format!("tei+{endpoint}"), "unused".into(), None, None).unwrap();
        assert_eq!(
            backend
                .score(
                    "q",
                    &[RerankDoc { text: "a" }],
                    Instant::now() + Duration::from_secs(2)
                )
                .unwrap(),
            vec![0.6]
        );
        assert_eq!(server.join().unwrap(), json!({"query":"q","texts":["a"]}));
    }
    #[test]
    fn remote_http_refusal_and_deadline() {
        // The refusal case gets a generous deadline: it checks how a 401 is
        // classified, and a loaded runner can take longer than 100 ms to
        // answer, which would turn it into a timeout. Only the slow-reply case
        // tests the deadline itself.
        for (status, delay, deadline) in [
            (401, Duration::ZERO, Duration::from_secs(10)),
            (200, Duration::from_millis(150), Duration::from_millis(100)),
        ] {
            let (endpoint, server) = serve("{}", status, delay);
            let backend = RemoteReranker::new(&endpoint, "m".into(), None, None).unwrap();
            let error = backend
                .score("q", &[RerankDoc { text: "a" }], Instant::now() + deadline)
                .unwrap_err();
            if status == 401 {
                assert!(matches!(error, RerankError::Refused(_)));
            } else {
                assert!(matches!(error, RerankError::Timeout));
            }
            server.join().unwrap();
        }
    }
    #[test]
    fn remote_configuration_limits_and_missing_credentials_do_not_connect() {
        let backend = RemoteReranker::new(
            "http://127.0.0.1:1",
            "m".into(),
            Some("AFT_RERANK_TEST_UNSET_KEY_04E05A91".into()),
            Some(1),
        )
        .unwrap();
        assert_eq!(backend.max_batch(), 1);
        assert_eq!(backend.fingerprint().backend, "remote");
        assert_eq!(backend.fingerprint().model, "m");
        assert!(matches!(
            backend.score(
                "q",
                &[RerankDoc { text: "a" }, RerankDoc { text: "b" }],
                Instant::now() + Duration::from_secs(1)
            ),
            Err(RerankError::Refused(_))
        ));
        assert!(
            matches!(backend.score("q", &[RerankDoc{text:"a"}], Instant::now()+Duration::from_secs(1)), Err(RerankError::Unavailable(message)) if message.contains("environment variable is unset"))
        );
        assert_eq!(
            RemoteReranker::new("http://127.0.0.1:1", "m".into(), None, None)
                .unwrap()
                .max_batch(),
            20
        );
        for endpoint in [
            "file:///tmp/model",
            "http://user:secret@localhost",
            "http://localhost?api_key=secret",
        ] {
            assert!(matches!(
                RemoteReranker::new(endpoint, "m".into(), None, None),
                Err(RerankError::Refused(_))
            ));
        }
    }

    #[test]
    fn remote_rejects_incomplete_duplicate_and_invalid_scores() {
        for value in [
            json!({"results":[]}),
            json!({"results":[{"index":0,"relevance_score":2}]}),
            json!({"results":[{"index":1,"relevance_score":0.5}]}),
            json!({"results":[{"index":0,"relevance_score":0.5},{"index":0,"relevance_score":0.5}]}),
        ] {
            assert!(matches!(
                parse_scores(&value, 1, false),
                Err(RerankError::Failed(_))
            ));
        }
    }
}
