//! Real proxy requests using routes authenticated as AFT's module identity.
//! A consent transport double records phone cards and supplies allow or decline.

use crate::secret::transport::fake::FakeClaustrum;
use crate::secret::ConsentOutcome;
use crate::secret_proxy::SecretProxy;
use aft_base::secret_proxy::tests::{Client, Server, K1, MARKER};
use serde_json::json;

use aft_base::secret_proxy::tests::vault_tests::{
    condition, live_config, record, Clock, PhoneGuard,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_vault_fetch_uses_the_concrete_target_and_holds_for_the_phone() {
    let daemon = FakeClaustrum::start();
    daemon.state.lock().unwrap().record = Some(record("query:key"));
    let phone = PhoneGuard::install();
    let server = Server::new("echo-request-h1").await;
    let cfg = live_config(&daemon, server.address.port(), Clock::new());
    let root = tempfile::tempdir().unwrap();
    let project = root.path().to_owned();
    let options = crate::url_fetch::UrlFetchOptions {
        allow_private: true,
        tls_config: Some(server.client_tls.clone()),
        ..Default::default()
    };
    let url = format!(
        "https://localhost:{}/approved/doc.md?key={MARKER}#local-fragment",
        server.address.port()
    );
    let port = server.address.port();
    let worker = std::thread::spawn(move || {
        crate::subc::with_secret_identity_for_test(
            &project,
            cfg.authorization_scope.stamp().clone(),
            || {
                let secrets = crate::url_fetch::SecretFetch {
                    vault: cfg.vault.clone(),
                    authorization: Some(cfg.authorization_scope.clone()),
                    labels: vec!["test".into()],
                    test_https_port: Some(port),
                    call_id: "fetch-live-vault".into(),
                    cancellation: Default::default(),
                };
                let result =
                    crate::url_fetch::fetch_url_with_secrets(&url, &project, options, &secrets);
                result.map(|path| std::fs::read_to_string(path).unwrap())
            },
        )
    });
    phone.phone.card(1).await;
    assert!(server.seen.lock().is_empty());
    phone.phone.decide(&daemon, 1, ConsentOutcome::Answered);
    let document = worker
        .join()
        .unwrap()
        .expect("the approved document was fetched");
    assert!(!document.contains(std::str::from_utf8(K1).unwrap()));
    assert!(document.contains(MARKER));
    let seen = server.seen.lock();
    assert_eq!(seen.len(), 1);
    assert!(seen[0]["target"]
        .as_str()
        .unwrap()
        .contains(std::str::from_utf8(K1).unwrap()));
    assert!(!seen[0]["target"]
        .as_str()
        .unwrap()
        .contains("local-fragment"));
    assert_eq!(daemon.state.lock().unwrap().calls, 2);
    assert_eq!(daemon.state.lock().unwrap().connections, 1);
    assert_eq!(
        daemon.state.lock().unwrap().params[0]["target"],
        json!({"scheme":"https", "host":"localhost", "port":443, "path":"/approved/doc.md"})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fetch_and_proxy_request_share_one_once_card_then_raise_exactly_one_successor() {
    let daemon = FakeClaustrum::start();
    {
        let mut state = daemon.state.lock().unwrap();
        state.record = Some(record("query:key"));
        state.once = true;
    }
    let phone = PhoneGuard::install();
    let server = Server::new("echo-request-h1").await;
    let cfg = live_config(&daemon, server.address.port(), Clock::new());
    let fetch_vault = cfg.vault.clone();
    let fetch_scope = cfg.authorization_scope.clone();
    let proxy = SecretProxy::start_with_tls(cfg, server.client_tls.clone()).unwrap();
    let mut client = Client::spawn(&proxy, &server);
    proxy.arm_command(client.process.id()).unwrap();
    client.connect(false);
    let proxy_worker = std::thread::spawn(move || {
        let reply = client.request(&format!("/approved?key={MARKER}"), json!({}), "");
        (reply, client)
    });
    phone.phone.card(1).await;
    let root = tempfile::tempdir().unwrap();
    let project = root.path().to_owned();
    let port = server.address.port();
    let options = crate::url_fetch::UrlFetchOptions {
        allow_private: true,
        tls_config: Some(server.client_tls.clone()),
        ..Default::default()
    };
    let fetch_worker = std::thread::spawn(move || {
        crate::subc::with_secret_identity_for_test(&project, fetch_scope.stamp().clone(), || {
            let secrets = crate::url_fetch::SecretFetch {
                vault: fetch_vault,
                authorization: Some(fetch_scope),
                labels: vec!["test".into()],
                test_https_port: Some(port),
                call_id: "concurrent-fetch".into(),
                cancellation: Default::default(),
            };
            crate::url_fetch::fetch_url_with_secrets(
                &format!("https://localhost:{port}/approved/doc.md?key={MARKER}"),
                &project,
                options,
                &secrets,
            )
        })
    });
    condition(|| daemon.state.lock().unwrap().calls >= 2).await;
    assert_eq!(phone.phone.state.lock().unwrap().requests.len(), 1);
    phone.phone.decide(&daemon, 1, ConsentOutcome::AnsweredOnce);
    phone.phone.card(2).await;
    condition(|| server.seen.lock().len() == 1).await;
    assert_eq!(
        server.seen.lock().len(),
        1,
        "one once approval can dispatch only one waiting request"
    );
    phone.phone.decide(&daemon, 2, ConsentOutcome::AnsweredOnce);
    let (reply, _client) = proxy_worker.join().unwrap();
    assert_eq!(reply["status"], 200);
    assert!(fetch_worker.join().unwrap().is_ok());
    assert_eq!(phone.phone.state.lock().unwrap().requests.len(), 2);
    assert_eq!(server.seen.lock().len(), 2);
    assert!(server.seen.lock().iter().all(|r| r["target"]
        .as_str()
        .unwrap()
        .contains(std::str::from_utf8(K1).unwrap())));
}
