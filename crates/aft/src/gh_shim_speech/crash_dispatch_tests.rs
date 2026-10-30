use super::*;

#[tokio::test]
async fn crash_after_bound_commit_child() {
    if std::env::var_os("AFT_TEST_BOUND_CRASH").is_none() {
        return;
    }
    let fixture = Fixture::new(false).await;
    let root = fixture.service.store.path().parent().unwrap().to_path_buf();
    fixture.plex.state.lock().unwrap().post_log = Some(root.join("provider-posted"));
    let mut params = fixture.params.clone();
    params["request_nonce"] = json!("crash-committed");
    // Captured stdout is a private parent/child pipe for the recovery capability;
    // the parent never prints or persists this payload in test logs.
    println!(
        "CRASH_SETUP {}",
        json!({"root":root,"head":fixture.head,"task":fixture.task,"session":fixture.session,"params":params})
    );
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    // Seed an old gh_speech_legacy_dispatch row; production no longer creates
    // these records, but a restart must still refuse to resend them.
    seed_historical_dispatch(&fixture, "crash-committed");
    println!("CRASH_AFTER_BOUND_COMMIT");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    std::process::exit(99);
}

struct NoSend;
impl RelayTransport for NoSend {
    fn opens_scoped_routes(&self) -> bool {
        true
    }
    async fn plexus(
        &self,
        _project: &str,
        _session: &str,
        _scope: Option<&subc_client_rs::ScopeSelector>,
        _body: Value,
    ) -> Result<Value, TransportError> {
        panic!("recovery must not silently send an already-queued attempt")
    }
}

struct Cleanup {
    root: PathBuf,
    head: ScopeStamp,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let stopped = crate::commands::bash_kill_scope::handle_bash_kill_scope_with_saved_tasks(
            json!({"scope":self.head}),
            Some(&self.head.owner),
            &[],
            Some(&self.root),
        );
        if stopped.success {
            let _ = fs::remove_dir_all(self.root.parent().unwrap());
        }
    }
}

#[tokio::test]
async fn historical_mailbox_after_process_crash_recovers_as_unknown_not_success() {
    let _lock = crate::test_env::process_env_lock();
    let output = tokio::task::spawn_blocking(|| {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "gh_shim::speech_dispatch_tests::crash::crash_after_bound_commit_child",
                "--nocapture",
            ])
            .env("AFT_TEST_BOUND_CRASH", "1")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(output.status.code(), Some(99));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("CRASH_AFTER_BOUND_COMMIT"));
    let setup = stdout
        .lines()
        .find_map(|line| line.split_once("CRASH_SETUP ").map(|(_, json)| json))
        .expect("missing private recovery payload");
    let setup: Value = serde_json::from_str(setup).unwrap();
    let root = PathBuf::from(setup["root"].as_str().unwrap());
    let head: ScopeStamp = serde_json::from_value(setup["head"].clone()).unwrap();
    let _cleanup = Cleanup {
        root: root.clone(),
        head: head.clone(),
    };
    let task = setup["task"].as_str().unwrap();
    let session = setup["session"].as_str().unwrap();
    let store = crate::db::gh_speech::Store::new(root.join("aft.db"));
    assert_eq!(store.pending_legacy_for_identity(session, task).unwrap(), 1);
    assert!(!root.join("provider-posted").exists());
    let service = crate::gh_shim_speech::service::record_service(&root);
    let mut routes = RouteScopes::default();
    routes.speech = Some(service);
    let answer = crate::gh_shim_relay::relay(
        &NoSend,
        RelayCall {
            operation: BOT_REQUEST_OPERATION,
            params: &setup["params"],
            first_party: true,
            route_scopes: &routes,
        },
        &|_| {},
    )
    .await;
    assert!(!answer.ok);
    assert_eq!(answer.data["refusal_code"], "outcome_unknown");
    assert_eq!(store.pending_legacy_for_identity(session, task).unwrap(), 1);
    assert!(!root.join("provider-posted").exists());
    let read = crate::gh_shim_relay::relay(
        &NoSend,
        RelayCall {
            operation: crate::gh_shim_relay::READ_REQUEST_OPERATION,
            params: &setup["params"],
            first_party: true,
            route_scopes: &routes,
        },
        &|_| {},
    )
    .await;
    assert_eq!(
        read.data["refusal_code"], "ticket_unknown",
        "terminal outcome lookup must not widen durable read redemption"
    );
    let stopped = crate::commands::bash_kill_scope::handle_bash_kill_scope_with_saved_tasks(
        json!({"scope":head}),
        Some(&head.owner),
        &[],
        Some(&root),
    );
    assert!(stopped.success, "{stopped:?}");
}
