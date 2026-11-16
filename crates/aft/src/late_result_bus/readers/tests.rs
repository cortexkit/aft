use super::*;
use std::sync::atomic::AtomicUsize;

struct FakeDelivery {
    body: Vec<u8>,
    msg_id: String,
    acks: AtomicUsize,
}
impl FakeDelivery {
    fn card(id: &str) -> Self {
        Self {
            body: serde_json::to_vec(
                &serde_json::json!({"elicitation_id":id,"feed":"answers","seq":1}),
            )
            .unwrap(),
            msg_id: format!("{id}:aft:1"),
            acks: AtomicUsize::new(0),
        }
    }
}
impl Delivery for FakeDelivery {
    fn body(&self) -> &[u8] {
        &self.body
    }
    fn msg_id(&self) -> &str {
        &self.msg_id
    }
    async fn ack(&self) -> Result<(), &'static str> {
        self.acks.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn bus_card_ack_follows_apply_and_replay_does_not_reapply() {
    let watch = watch_card("el_bus_apply");
    let event = FakeDelivery::card("el_bus_apply");
    let delivered = apply_delivery(&event, Kind::Cards);
    tokio::pin!(delivered);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut delivered)
            .await
            .is_err()
    );
    assert_eq!(
        event.acks.load(Ordering::SeqCst),
        0,
        "read ownership alone is not an apply receipt"
    );
    let applies = AtomicUsize::new(0);
    applies.fetch_add(1, Ordering::SeqCst);
    watch.receipt().applied();
    delivered.await.unwrap();
    assert_eq!(event.acks.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_secs(1), apply_delivery(&event, Kind::Cards))
        .await
        .unwrap()
        .unwrap();
    let mut duplicate = FakeDelivery::card("el_bus_apply");
    duplicate.msg_id = "a-new-transport-id".into();
    tokio::time::timeout(
        Duration::from_secs(1),
        apply_delivery(&duplicate, Kind::Cards),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        watch.wake.pending.lock().is_empty(),
        "body pair, not message id, deduplicates an applied event"
    );
    drop(watch);
    assert_eq!(applies.load(Ordering::SeqCst), 1);
    assert_eq!(event.acks.load(Ordering::SeqCst), 2);
    assert_eq!(duplicate.acks.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn bus_card_mismatched_id_uses_body_and_malformed_is_acked() {
    let watch = watch_card("el_bus_body");
    let mut event = FakeDelivery::card("el_bus_body");
    event.msg_id = "unrelated:aft".into();
    let delivered = apply_delivery(&event, Kind::Cards);
    tokio::pin!(delivered);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut delivered)
            .await
            .is_err()
    );
    assert_eq!(watch.wake.pending.lock().len(), 1);
    watch.receipt().applied();
    delivered.await.unwrap();
    let malformed = FakeDelivery {
        body: b"not json".to_vec(),
        msg_id: "bad".into(),
        acks: AtomicUsize::new(0),
    };
    apply_delivery(&malformed, Kind::Cards).await.unwrap();
    assert_eq!(malformed.acks.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn bus_card_attached_backstop_and_detach_interrupt() {
    let attachment = TestAttachment::new("el_bus_schedule", true);
    let watch = Arc::new(watch_card("el_bus_schedule"));
    let owned = watch.clone();
    let wait = tokio::spawn(async move { owned.wait().await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(29)).await;
    assert!(!wait.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    wait.await.unwrap();
    let owned = watch.clone();
    let wait = tokio::spawn(async move { owned.wait().await });
    tokio::task::yield_now().await;
    attachment.state.send_replace(false);
    wait.await.unwrap();
    assert!(!watch.attached());
    watch.wait().await;
    assert_eq!(BACKSTOP, Duration::from_secs(600));
}

#[test]
fn bus_send_state_decoder_ignores_state_and_accepts_no_identity() {
    assert_eq!(
        send_id(br#"{"send_id":"send-7","state":"delivered"}"#).as_deref(),
        Some("send-7")
    );
    for body in [
        b"{}".as_slice(),
        b"[]",
        b"null",
        b"bad",
        br#"{"send_id":7}"#,
    ] {
        assert_eq!(send_id(body), None);
    }
}

#[test]
fn bus_card_settled_cingulate_vectors_pin_subject_body_and_headers() {
    const BODY: &[u8] = include_bytes!("../../../tests/fixtures/card_settled_v1/aft.aft.body.json");
    const HEADERS: &[u8] =
        include_bytes!("../../../tests/fixtures/card_settled_v1/aft.aft.headers.json");
    const SUBJECT: &str =
        include_str!("../../../tests/fixtures/card_settled_v1/aft.aft.subject.txt");
    let names = cortexkit_bus_naming::AccountNames::derive("box_example").unwrap();
    let parsed = names.parse_event_subject(SUBJECT).unwrap();
    assert_eq!(parsed.module, "cingulate");
    assert_eq!(parsed.event, "card_settled");
    assert_eq!(parsed.version, 1);
    assert_eq!(parsed.reader.as_deref(), Some(CARD_ID_READER));
    assert_eq!(
        names
            .event_subject_with_reader(
                &parsed.module,
                &parsed.event,
                parsed.version,
                parsed.reader.as_deref().unwrap()
            )
            .unwrap(),
        SUBJECT
    );
    let row = EVENT_CONSUMERS
        .iter()
        .find(|row| {
            row.module_id == "aft"
                && row
                    .filter_subjects
                    .iter()
                    .any(|filter| filter.contains("card_settled"))
        })
        .unwrap();
    assert_eq!(
        names
            .event_subject_with_reader("cingulate", "card_settled", 1, row.module_id)
            .unwrap(),
        SUBJECT
    );
    assert_eq!(
        BODY,
        br#"{"elicitation_id":"el_1","feed":"answers","seq":1}"#
    );
    let body: CardEvent = serde_json::from_slice(BODY).unwrap();
    assert_eq!(
        (body.elicitation_id.as_str(), body.feed.as_str(), body.seq),
        ("el_1", "answers", 1)
    );
    let headers: serde_json::Value = serde_json::from_slice(HEADERS).unwrap();
    assert_eq!(HEADERS, br#"{"Nats-Msg-Id":"el_1:aft:1"}"#);
    assert_eq!(
        headers["Nats-Msg-Id"],
        format!("{}:{CARD_ID_READER}:{}", body.elicitation_id, body.seq)
    );
}

#[test]
fn bus_health_counters_are_bounded_inside_metrics_budget() {
    let mut health = super::super::BusHealth::default();
    health.cards = ReaderHealth {
        reader_attached: true,
        reason: Some("card_message_id_mismatch"),
        events_handled: u64::MAX,
        malformed_events: u64::MAX,
        mismatched_ids: u64::MAX,
        ..Default::default()
    };
    health.sendstate = ReaderHealth {
        reader_attached: false,
        reason: Some("send_state_body_malformed"),
        events_handled: u64::MAX,
        malformed_events: u64::MAX,
        mismatched_ids: u64::MAX,
        ..Default::default()
    };
    let bytes = serde_json::to_vec(&health).unwrap();
    assert!(
        bytes.len() < 1024,
        "bus metrics must leave space in the 12 KiB health envelope: {}",
        bytes.len()
    );
    assert!(!String::from_utf8(bytes).unwrap().contains("warned"));
}
