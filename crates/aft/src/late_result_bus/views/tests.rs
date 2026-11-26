use super::*;

#[derive(Default)]
struct RecordingWriter(Vec<(String, Vec<u8>)>);
impl Writer for RecordingWriter {
    async fn put(&mut self, key: &str, value: Vec<u8>) -> Result<(), &'static str> {
        self.0.push((key.into(), value));
        Ok(())
    }
}

#[tokio::test]
async fn notice_records_gate_opens_only_after_every_notice_view_put_is_acknowledged() {
    struct RejectLast(usize);
    impl Writer for RejectLast {
        async fn put(&mut self, _key: &str, _value: Vec<u8>) -> Result<(), &'static str> {
            self.0 -= 1;
            if self.0 == 0 {
                Err("put_not_acknowledged")
            } else {
                Ok(())
            }
        }
    }

    let notice_views: Vec<_> = crate::subc::tool_views::bus_snapshot()
        .iter()
        .filter(|(pin, _)| pin.starts_with("nv1:"))
        .cloned()
        .collect();
    assert_eq!(notice_views.len(), 3);
    let ready = NoticeViewsReady::default();
    let mut publication = Publication::new(&ready);
    let mut writer = RejectLast(notice_views.len());
    assert_eq!(
        publication.sync(&mut writer, &notice_views).await,
        Err("put_not_acknowledged")
    );
    assert!(!ready.wait(Duration::ZERO).await);

    let mut writer = RecordingWriter::default();
    publication.sync(&mut writer, &notice_views).await.unwrap();
    assert_eq!(writer.0.len(), 1, "only the unacknowledged view is retried");
    assert!(
        ready.wait(Duration::ZERO).await,
        "acknowledgments reopen admission even after a timed-out send"
    );
}

#[tokio::test]
async fn oversized_notice_view_disables_pending_notice_publication() {
    use futures_util::FutureExt;

    let mut writer = RecordingWriter::default();
    let ready = NoticeViewsReady::default();
    let mut publication = Publication::new(&ready);
    let notice_pin = crate::subc::tool_views::notice_view_pins().next().unwrap();
    publication
        .sync(
            &mut writer,
            &[(
                notice_pin.clone(),
                vec![b'x'; declaration().max_value_bytes as usize + 1],
            )],
        )
        .await
        .unwrap();
    assert!(writer.0.is_empty());
    assert!(ready
        .next_pending()
        .now_or_never()
        .expect("oversized notice view should permanently close the gate")
        .is_none());
}

#[tokio::test]
async fn bus_views_bytes_match_every_served_pin_and_full_pin_key() {
    use crate::subc::tool_views;
    use cortexkit_role_tool_provider::views::{ViewEntry, ViewsReply, ViewsRequest};

    let snapshot = tool_views::bus_snapshot();
    let costs = |legacy_only: bool| {
        snapshot
            .iter()
            .filter(|(pin, _)| !legacy_only || !pin.starts_with("tp2:"))
            .fold((0, 0, 0), |(count, bytes, largest), (pin, body)| {
                (
                    count + 1,
                    bytes + key(pin).len() + body.len(),
                    largest.max(body.len()),
                )
            })
    };
    let (before_keys, before_bytes, _) = costs(true);
    let (keys, bytes, largest) = costs(false);
    assert_eq!(before_keys, tool_views::SHIPPED.len() + 3);
    assert!(keys > before_keys);
    let bounds = declaration();
    assert!(largest <= bounds.max_value_bytes as usize);
    assert!((bytes as u64) * u64::from(bounds.history) <= bounds.max_bytes);
    println!(
        "AFT view-bus snapshot (hex keys + values): {before_keys} keys / {before_bytes} bytes before; {keys} keys / {bytes} bytes after; largest value {largest} bytes"
    );

    let mut writer = RecordingWriter::default();
    let ready = NoticeViewsReady::default();
    let mut publication = Publication::new(&ready);
    publication.sync(&mut writer, snapshot).await.unwrap();
    assert_eq!(writer.0.len(), keys);
    for (pin, _) in snapshot.iter().filter(|(pin, _)| pin.starts_with("tp")) {
        let request = ViewsRequest::new(vec![pin.clone()]);
        let reply = ViewsReply::new(std::collections::BTreeMap::from([(
            pin.clone(),
            ViewEntry::Manifest(tool_views::view(pin).unwrap().into_owned()),
        )]));
        reply.validate_for(&request).unwrap();
        let route_bytes = serde_json::to_vec(&serde_json::to_value(reply).unwrap()).unwrap();
        assert_eq!(
            writer
                .0
                .iter()
                .find(|(stored, _)| stored == &key(pin))
                .unwrap()
                .1,
            route_bytes
        );
        assert!(key(pin)
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
    }
    let pin = "tp1:apply_patch:fa62c9797e5d14436714f5630d15815d2adc1185023a3dcba3f3ed20fc9f0548:1";
    assert!(tool_views::SHIPPED.contains_key(pin));
    let versioned = pin.replacen("tp1:", "tp2:", 1);
    assert!(writer
        .0
        .iter()
        .any(|(stored, _)| stored == &key(&versioned)));
    assert_eq!(key(pin), "7470313a6170706c795f70617463683a666136326339373937653564313434333637313466353633306431353831356432616463313138353032336133646362613366336564323066633966303534383a31");
    assert_ne!(key(pin), key(&pin.replace(":1", ":2")));
    let writes = writer.0.len();
    publication.sync(&mut writer, snapshot).await.unwrap();
    assert_eq!(writer.0.len(), writes);
}

#[tokio::test]
async fn notice_records_bus_views_and_tool_views_decode_as_the_same_one_pin_reply() {
    use cortexkit_role_tool_provider::views::{ViewEntry, ViewsReply, ViewsRequest};
    use serde_json::json;

    let mut writer = RecordingWriter::default();
    Publication::new(&NoticeViewsReady::default())
        .sync(&mut writer, crate::subc::tool_views::bus_snapshot())
        .await
        .unwrap();
    for name in ["bash_finished", "bash_still_running", "bash_watch"] {
        let pin = format!("nv1:aft:{name}:1");
        let route = crate::subc::tool_views::answer(json!({"pins":[pin]})).unwrap();
        let bytes = &writer
            .0
            .iter()
            .find(|(stored, _)| stored == &key(&pin))
            .unwrap()
            .1;
        let decoded: ViewsReply = serde_json::from_slice(bytes).unwrap();
        decoded
            .validate_for(&ViewsRequest::new(vec![pin.clone()]))
            .unwrap();
        assert_eq!(serde_json::to_value(&decoded).unwrap(), route);
        let ViewEntry::Notice(view) = &decoded.views[&pin] else {
            panic!("missing {pin}");
        };
        view.check_pin("aft", &pin).unwrap();
        assert_eq!(view.notice.name, name);
        assert_eq!(view.bucket_key("aft").unwrap(), key(&pin));
    }
    for pin in [
        "nv1:other:bash_finished:1",
        "nv1:aft:bash_finished:01",
        "nv1:aft:bash_finished:9007199254740992",
    ] {
        assert_eq!(
            crate::subc::tool_views::answer(json!({"pins":[pin]})).unwrap()["views"][pin],
            "unknown"
        );
    }
}

#[tokio::test]
async fn bus_views_oversized_is_skipped_not_truncated_and_changes_republish() {
    let mut writer = RecordingWriter::default();
    let ready = NoticeViewsReady::default();
    let mut publication = Publication::new(&ready);
    let cap = declaration().max_value_bytes as usize;
    publication
        .sync(
            &mut writer,
            &[
                ("oversize".into(), vec![b'x'; cap + 1]),
                ("at-limit".into(), vec![b'y'; cap]),
            ],
        )
        .await
        .unwrap();
    assert_eq!(publication.skipped.len(), 1);
    assert_eq!(writer.0, vec![(key("at-limit"), vec![b'y'; cap])]);
    publication
        .sync(&mut writer, &[("at-limit".into(), b"revised".to_vec())])
        .await
        .unwrap();
    assert_eq!(writer.0[1], (key("at-limit"), b"revised".to_vec()));
}
