use super::*;

struct Answer(ConsentOutcome);
impl crate::secret::SecretConsentTransport for Answer {
    fn file(&self, _: &SecretConsentRequest) -> ConsentOutcome {
        self.0.clone()
    }
    fn withhold(&self, _: &SecretConsentRequest) -> ConsentOutcome {
        ConsentOutcome::Cancelled
    }
}

async fn exact_line(scope_ref: &str, outcome: ConsentOutcome, expected: &str) {
    let _transport = TransportGuard::install(Some(Arc::new(Answer(outcome))));
    let coordinator = crate::secret::requests::RequestSecrets::new(
        Arc::new(crate::secret::UnavailableVault),
        SecretScope::from_stamp(&stamp(scope_ref, "head")),
        Some((head().project_root.clone(), "fetch".into())),
    );
    let end = crate::secret::vault::now_ms() + 600_000;
    let reason = coordinator
        .approve(
            "repo",
            &target(),
            crate::secret::ApprovalRequest {
                request_id: "01HF7YAT000000000000000001".into(),
                statement_b64url: "c3RhdGVtZW50".into(),
                expires_at_ms: end,
            },
            end,
        )
        .await
        .unwrap_err();
    let LabelOutcome::Refuse { code, message } =
        fetch_consent_refusal("repo", "x.test", reason, &Default::default())
    else {
        panic!("a refused fetch")
    };
    assert_eq!(code, crate::secret::SECRET_ACCESS_UNAVAILABLE);
    assert_eq!(message, expected);
    #[cfg(unix)]
    assert!(
        coordinator.status().is_empty(),
        "terminal outcomes clear the approval status"
    );
    assert_eq!(
        coordinator
            .activity()
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declined_fetch_uses_the_exact_approved_line() {
    exact_line(
        "fetch-declined",
        ConsentOutcome::Declined,
        "secret repo: approval was declined for x.test; the request was not sent",
    )
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_fetch_uses_the_exact_approved_line() {
    exact_line(
        "fetch-expired",
        ConsentOutcome::Expired,
        "secret repo: approval expired for x.test; the request was not sent",
    )
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fetch_that_could_not_file_uses_the_exact_approved_line() {
    exact_line(
        "fetch-unfiled",
        consent_unavailable("Cingulate could not file the secret-use card"),
        "secret repo: the approval request for x.test could not be filed; the request was not sent",
    )
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_fetch_uses_the_exact_approved_line() {
    exact_line(
        "fetch-cancelled",
        ConsentOutcome::Cancelled,
        "secret repo: the request for x.test was cancelled; the request was not sent",
    )
    .await;
}

#[test]
fn each_approved_fetch_line_scrubs_both_name_and_host_against_an_earlier_release() {
    const KEY: &str = "already-released-key-0123456789";
    let mut held = crate::url_fetch::HeldKeys::default();
    held.hold("first", KEY.as_bytes()).unwrap();
    for reason in [
        crate::secret::requests::UseError::Declined,
        crate::secret::requests::UseError::Expired,
        crate::secret::requests::UseError::Cancelled,
        crate::secret::requests::UseError::Unavailable,
    ] {
        let LabelOutcome::Refuse { message, .. } =
            fetch_consent_refusal(KEY, &format!("{KEY}.test"), reason, &held)
        else {
            panic!("a refused fetch")
        };
        assert!(!message.contains(KEY));
        assert!(message.starts_with("secret {{ck-secret:first}}:"));
        assert!(message.contains("{{ck-secret:first}}.test; the request was not sent"));
    }
}
