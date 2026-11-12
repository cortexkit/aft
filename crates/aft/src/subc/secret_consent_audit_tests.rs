use super::*;

#[test]
fn answered_records_require_an_explicit_per_request_decision() {
    let id = "01HF7YAT000000000000000001";
    for answer in [
        json!(null),
        json!({}),
        json!({"decision":"future"}),
        json!({"decision":false}),
        json!({"decision":"allow", "items":null}),
        json!({"decision":"allow", "items":{}}),
        json!({"decision":"allow", "items":[]}),
        json!({"items":[{"request_id":id,"decision":"future"}]}),
        json!({"items":[{"request_id":id,"decision":"allow"},{"request_id":id,"decision":"decline"}]}),
    ] {
        let record = json!({"kind":"secret_use","state":"answered","secret_use_answer":answer});
        assert_eq!(
            decode_secret_record(&record, id),
            ConsentOutcome::Indeterminate,
            "{record}"
        );
    }
    let record =
        json!({"kind":"secret_use","state":"answered","secret_use_answer":{"decision":"decline"}});
    assert_eq!(decode_secret_record(&record, id), ConsentOutcome::Declined);
}

struct UnreadableAnswerEndpoint {
    calls: StdMutex<Vec<super::super::super::consent::ConsentCall>>,
}
impl super::super::super::consent::ConsentTransport for UnreadableAnswerEndpoint {
    fn call(
        &self,
        call: super::super::super::consent::ConsentCall,
    ) -> super::super::super::consent::ConsentFuture {
        self.calls.lock().unwrap().push(call.clone());
        Box::pin(async move {
            match call.method {
                "consent.request" => Ok(json!({"elicitation_id":"el_unreadable"})),
                "consent.await" => Ok(
                    json!({"kind":"secret_use","state":"answered","secret_use_answer":{"decision":"future"}}),
                ),
                other => panic!("unexpected call after unreadable answered record: {other}"),
            }
        })
    }
}

#[tokio::test]
async fn an_unreadable_answer_never_becomes_a_cached_person_decision() {
    let endpoint = Arc::new(UnreadableAnswerEndpoint {
        calls: Default::default(),
    });
    let transport = SubcSecretConsent {
        transport: endpoint.clone(),
        runtime: tokio::runtime::Handle::current(),
        cards: Default::default(),
    };
    let request = held_request(crate::secret::vault::now_ms() + 600_000);
    for _ in 0..2 {
        assert_eq!(
            transport
                .file_cancellable(&request, SECRET_AWAIT_CAP, Some(&Default::default()))
                .await,
            ConsentOutcome::Indeterminate
        );
        let cards = transport.cards.lock().await;
        let card = cards
            .get(&(request.scope.clone(), request.request_id.clone()))
            .unwrap()
            .lock()
            .await;
        assert!(
            card.answered.is_none(),
            "a malformed answer is not a decision"
        );
        assert!(card.warned_unreadable);
    }
    let calls = endpoint.calls.lock().unwrap();
    assert_eq!(
        calls.iter().map(|call| call.method).collect::<Vec<_>>(),
        ["consent.request", "consent.await", "consent.await"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_indeterminate_shared_outcome_is_evicted_and_uses_the_vault_unavailable_refusal() {
    struct Answers(std::sync::atomic::AtomicUsize);
    impl crate::secret::SecretConsentTransport for Answers {
        fn file(&self, _: &SecretConsentRequest) -> ConsentOutcome {
            if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                ConsentOutcome::Indeterminate
            } else {
                ConsentOutcome::Declined
            }
        }
        fn withhold(&self, _: &SecretConsentRequest) -> ConsentOutcome {
            ConsentOutcome::Cancelled
        }
    }
    let answer = Arc::new(Answers(Default::default()));
    let _installed = TransportGuard::install(Some(answer.clone()));
    let request = held_request(crate::secret::vault::now_ms() + 600_000);
    assert_eq!(
        crate::secret::consent::wait_consent(request.clone()).await,
        ConsentOutcome::Indeterminate
    );
    assert_eq!(
        crate::secret::consent::wait_consent(request).await,
        ConsentOutcome::Declined
    );
    assert_eq!(
        answer.0.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "do not reuse an indeterminate result as a person's cached decline"
    );
    let refusal = fetch_consent_refusal(
        "test",
        "x.test",
        crate::secret::requests::UseError::Indeterminate,
        &Default::default(),
    );
    assert_eq!(
        refusal,
        crate::secret::outcome_for("test", &crate::secret::UseAnswer::Unavailable)
    );
}

struct CancelledLostFiling {
    calls: StdMutex<Vec<super::super::super::consent::ConsentCall>>,
    cancel: crate::secret::consent::Cancellation,
    garbled: bool,
}
impl super::super::super::consent::ConsentTransport for CancelledLostFiling {
    fn call(
        &self,
        call: super::super::super::consent::ConsentCall,
    ) -> super::super::super::consent::ConsentFuture {
        assert_eq!(call.method, "consent.request");
        self.calls.lock().unwrap().push(call);
        let cancel = self.cancel.clone();
        let garbled = self.garbled;
        Box::pin(async move {
            cancel.cancel();
            if garbled {
                Ok(json!({"accepted":true}))
            } else {
                Err(super::super::super::consent::ConsentError::Unavailable(
                    "lost filing reply".into(),
                ))
            }
        })
    }
}

#[tokio::test]
async fn cancellation_after_a_lost_or_garbled_filing_never_creates_a_recovery_call() {
    for garbled in [false, true] {
        let cancel = crate::secret::consent::Cancellation::default();
        let endpoint = Arc::new(CancelledLostFiling {
            calls: Default::default(),
            cancel: cancel.clone(),
            garbled,
        });
        let transport = Arc::new(SubcSecretConsent {
            transport: endpoint.clone(),
            runtime: tokio::runtime::Handle::current(),
            cards: Default::default(),
        });
        let deadline = crate::secret::vault::now_ms() + 600_000;
        let request = held_request(deadline);
        assert_eq!(
            transport.clone().wait(request.clone(), cancel).await,
            ConsentOutcome::Cancelled
        );
        assert_eq!(
            transport
                .file_cancellable(&request, SECRET_AWAIT_CAP, None)
                .await,
            ConsentOutcome::Cancelled
        );
        let calls = endpoint.calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "cancellation must not create an identical replay, even to recover an unknown id"
        );
        assert!(
            calls[0].params["expires_at_ms"].as_u64().unwrap() <= deadline,
            "the unknown-id card must expire by the original request deadline"
        );
    }
}

#[test]
fn production_card_withdrawal_uses_the_daemon_runtime_after_the_request_runtime_stops() {
    struct Endpoint {
        started: tokio::sync::Notify,
        withdrawn: tokio::sync::Notify,
        calls: StdMutex<Vec<(&'static str, String)>>,
    }
    impl super::super::super::consent::ConsentTransport for Endpoint {
        fn call(
            &self,
            call: super::super::super::consent::ConsentCall,
        ) -> super::super::super::consent::ConsentFuture {
            self.calls.lock().unwrap().push((
                call.method,
                format!("{:?}", tokio::runtime::Handle::current().id()),
            ));
            let response = match call.method {
                "consent.request" => Some(json!({"elicitation_id":"el_runtime"})),
                "consent.await" => {
                    self.started.notify_one();
                    None
                }
                "consent.withdraw" => {
                    self.withdrawn.notify_one();
                    Some(json!({}))
                }
                other => panic!("unexpected call {other}"),
            };
            Box::pin(async move {
                match response {
                    Some(reply) => Ok(reply),
                    None => std::future::pending().await,
                }
            })
        }
    }
    let _installed = TransportGuard::install(None);
    let daemon = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let endpoint = Arc::new(Endpoint {
        started: Default::default(),
        withdrawn: Default::default(),
        calls: Default::default(),
    });
    // Exercise the production installer: it must capture the daemon handle,
    // not the short-lived proxy/request runtime that later joins the card.
    daemon.block_on(async {
        install_subc_transport(endpoint.clone());
    });
    let request_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    request_runtime.block_on(async {
        let joined = crate::secret::consent::wait_consent(held_request(
            crate::secret::vault::now_ms() + 600_000,
        ));
        tokio::pin!(joined);
        tokio::select! {
            () = endpoint.started.notified() => {},
            _ = &mut joined => panic!("the phone card is still pending"),
            () = tokio::time::sleep(Duration::from_secs(2)) => panic!("the card was not filed"),
        }
    });
    drop(request_runtime);
    daemon.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), endpoint.withdrawn.notified())
            .await
            .unwrap();
    });
    let calls = endpoint.calls.lock().unwrap();
    assert_eq!(
        calls.iter().map(|(method, _)| *method).collect::<Vec<_>>(),
        ["consent.request", "consent.await", "consent.withdraw"]
    );
    assert!(calls
        .iter()
        .all(|(_, runtime)| runtime == &format!("{:?}", daemon.handle().id())));
}
