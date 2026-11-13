use super::*;
use crate::secret_proxy::diagnostics::{log_refusal, Reason};

const PRIVATE_HEADER: &str = "private-header-value-do-not-log";

fn assert_safe_line(line: &str, code: &str) {
    assert!(line.contains(&format!("reason={code} ")), "{line}");
    assert!(line.contains("label=\"hetzner\""), "{line}");
    assert!(line.contains("host=\"api.hetzner.cloud\""), "{line}");
    assert!(!line.contains(std::str::from_utf8(K1).unwrap()), "{line}");
    assert!(!line.contains(PRIVATE_HEADER), "{line}");
    assert!(!line.contains("/v1/locations"), "{line}");
    assert_eq!(line.lines().count(), 1, "{line}");
}

macro_rules! reason_test {
    ($name:ident, $reason:ident, $code:literal) => {
        #[test]
        fn $name() {
            let proxy =
                SecretProxy::start(config(Vault::new(), 443, &[("hetzner", Location::Bearer)]))
                    .unwrap();
            proxy.state.install("hetzner", K1).unwrap();
            let (_, lines) = crate::logging::capture_log_lines(|| {
                log_refusal(
                    &proxy.state,
                    Reason::$reason,
                    "hetzner",
                    "api.hetzner.cloud",
                );
            });
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert_safe_line(&lines[0], $code);
        }
    };
}

reason_test!(logs_request_limit, RequestLimit, "request_limit");
reason_test!(logs_stopped, Stopped, "stopped");
reason_test!(
    logs_generation_invalid,
    GenerationInvalid,
    "generation_invalid"
);
reason_test!(logs_host_mismatch, HostMismatch, "host_mismatch");
reason_test!(logs_invalid_request, InvalidRequest, "invalid_request");
reason_test!(logs_unscanned_label, UnscannedLabel, "unscanned_label");
reason_test!(logs_body_invalid, BodyInvalid, "body_invalid");
reason_test!(
    logs_upstream_connect_timeout,
    UpstreamConnectTimeout,
    "upstream_connect_timeout"
);
reason_test!(logs_upstream_connect, UpstreamConnect, "upstream_connect");
reason_test!(logs_upstream_tls, UpstreamTls, "upstream_tls");
reason_test!(logs_upstream_http, UpstreamHttp, "upstream_http");
reason_test!(
    logs_vault_unavailable,
    VaultUnavailable,
    "vault_unavailable"
);
reason_test!(
    logs_location_mismatch,
    LocationMismatch,
    "location_mismatch"
);
reason_test!(logs_material_invalid, MaterialInvalid, "material_invalid");
reason_test!(logs_material_install, MaterialInstall, "material_install");
reason_test!(logs_audit_failed, AuditFailed, "audit_failed");
reason_test!(logs_upstream_send, UpstreamSend, "upstream_send");
reason_test!(logs_response_invalid, ResponseInvalid, "response_invalid");
reason_test!(logs_scrub_failed, ScrubFailed, "scrub_failed");
reason_test!(logs_connect_method, ConnectMethod, "connect_method");
reason_test!(logs_proxy_auth, ProxyAuth, "proxy_auth");
reason_test!(
    logs_connect_authority,
    ConnectAuthority,
    "connect_authority"
);
reason_test!(
    logs_tls_configuration,
    TlsConfiguration,
    "tls_configuration"
);

#[test]
fn diagnostic_metadata_is_scrubbed_and_quoted() {
    let proxy =
        SecretProxy::start(config(Vault::new(), 443, &[("hetzner", Location::Bearer)])).unwrap();
    proxy.state.install("hetzner", K1).unwrap();
    let key = std::str::from_utf8(K1).unwrap();
    let (_, lines) = crate::logging::capture_log_lines(|| {
        log_refusal(
            &proxy.state,
            Reason::HostMismatch,
            key,
            &format!("{key}\n.example"),
        );
    });
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(!lines[0].contains(key), "{lines:?}");
    assert!(lines[0].contains("{{ck-secret:hetzner}}"), "{lines:?}");
    assert_eq!(lines[0].lines().count(), 1, "{lines:?}");
    assert!(lines[0].contains("\\n"), "{lines:?}");
}

// Vault storage is not decoded by the proxy. Model a legacy whole-host secret
// here; the proxy receives only a location and key from SecretVault.
struct LegacyHostVault {
    host: &'static str,
    port: Option<u16>,
    path_prefix: Option<&'static str>,
    location: &'static str,
    scope_json: &'static str,
    calls: AtomicUsize,
}

impl SecretVault for LegacyHostVault {
    fn use_secret(&self, _: &SecretScope, label: &str, target: &ApiKeyTarget) -> UseAnswer {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(label, "hetzner");
        assert_eq!(target.scheme, "https");
        assert_eq!(target.host, self.host);
        assert_eq!(target.port, Some(self.port.unwrap_or(443)));
        assert_eq!(target.path, "/v1/locations");
        assert_eq!(self.path_prefix, None);
        assert_eq!(self.scope_json, "");
        UseAnswer::Released(ReleasedApiKey::new(
            self.location.into(),
            Material::new(K1.to_vec()),
        ))
    }

    fn audit(&self, _: &secret_rules::audit::AuditRecordV1) -> Result<(), AuditError> {
        Ok(())
    }
}

fn legacy_vault() -> Arc<LegacyHostVault> {
    Arc::new(LegacyHostVault {
        host: "api.hetzner.cloud",
        port: None,
        path_prefix: None,
        location: "authorization_bearer",
        scope_json: "",
        calls: AtomicUsize::new(0),
    })
}

fn curl_request() -> Value {
    json!({
        "op": "request",
        "method": "GET",
        "authority": "api.hetzner.cloud",
        "omit_host": true,
        "path": "/v1/locations",
        "headers": {
            "authorization": "Bearer {{ck-secret:hetzner}}",
            "x-private-test": PRIVATE_HEADER,
            "user-agent": "curl/offline-test"
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn curl_shaped_legacy_whole_host_bearer_reaches_vault_over_h1_and_h2() {
    for (h2, omit_host) in [(false, false), (true, false), (true, true)] {
        let server = Server::new("echo-request").await;
        let vault = legacy_vault();
        let mut cfg = config(Vault::new(), 443, &[("hetzner", Location::Bearer)]);
        cfg.vault = vault.clone();
        let proxy = SecretProxy::start_with_tls(cfg, server.client_tls.clone()).unwrap();
        proxy
            .state
            .test_upstream
            .lock()
            .insert("api.hetzner.cloud".into(), server.address);
        let mut client = Client::spawn(&proxy, &server);
        proxy.arm_command(client.process.id()).unwrap();
        assert_eq!(
            client.command(json!({"op":"connect", "authority":"api.hetzner.cloud:443", "h2":h2}))
                ["ok"],
            true
        );
        let mut request = curl_request();
        request["omit_host"] = json!(omit_host);
        let response = client.command(request);
        assert_eq!(response["status"], 200, "{response}");
        assert_eq!(vault.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            server.seen.lock().last().unwrap()["headers"]["authorization"],
            "Bearer test-key-12345678"
        );
        assert!(proxy.state.test_refusals.lock().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn curl_shaped_unavailable_vault_reproduces_generic_refusal_exactly() {
    let server = Server::new("echo-request").await;
    let mut cfg = config(Vault::new(), 443, &[("hetzner", Location::Bearer)]);
    cfg.vault = Arc::new(crate::secret::UnavailableVault);
    let proxy = SecretProxy::start_with_tls(cfg, server.client_tls.clone()).unwrap();
    proxy
        .state
        .test_upstream
        .lock()
        .insert("api.hetzner.cloud".into(), server.address);
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    assert_eq!(
        client.command(json!({"op":"connect", "authority":"api.hetzner.cloud:443", "h2":true}))
            ["ok"],
        true
    );
    let response = client.command(curl_request());
    assert_eq!(response["status"], 403, "{response}");
    assert_eq!(
        response["body"],
        "Secret proxy refused the request: hetzner (api.hetzner.cloud)\n"
    );
    assert!(server.seen.lock().is_empty());
    let lines = proxy.state.test_refusals.lock();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_safe_line(&lines[0], "vault_unavailable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn curl_shaped_upstream_tls_failure_refuses_before_vault_and_logs_once() {
    let server = Server::new("wrong-name").await;
    let vault = legacy_vault();
    let mut cfg = config(Vault::new(), 443, &[("hetzner", Location::Bearer)]);
    cfg.vault = vault.clone();
    let proxy = SecretProxy::start_with_tls(cfg, server.client_tls.clone()).unwrap();
    proxy
        .state
        .test_upstream
        .lock()
        .insert("api.hetzner.cloud".into(), server.address);
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    assert_eq!(
        client.command(json!({"op":"connect", "authority":"api.hetzner.cloud:443", "h2":true}))
            ["ok"],
        true
    );
    let response = client.command(curl_request());
    assert_eq!(response["status"], 403, "{response}");
    assert_eq!(
        response["body"],
        "Secret proxy refused the request: hetzner (api.hetzner.cloud)\n"
    );
    assert_eq!(vault.calls.load(Ordering::SeqCst), 0);
    assert!(server.seen.lock().is_empty());
    let lines = proxy.state.test_refusals.lock();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_safe_line(&lines[0], "upstream_tls");
}
