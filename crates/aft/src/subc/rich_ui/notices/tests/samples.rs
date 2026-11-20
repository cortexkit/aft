use super::*;
use cortexkit_role_tool_display::{parse_notice_view, parse_notice_view_key, NoticeRecord};
use cortexkit_role_tool_provider::views::{ViewEntry, ViewsReply};
use serde_json::Value;

fn read(bytes: &[u8], sha256: &str) -> Value {
    assert_eq!(format!("{:x}", Sha256::digest(bytes)), sha256);
    serde_json::from_slice(bytes).unwrap()
}

#[test]
fn notice_records_match_occipital_phone_reader_sample_contract() {
    let sample = read(
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/notice-records/02-wake-records-with-notices.json"
        )),
        "f6a3c1c6df70a10a79b1abdbc2c4f59e17b8e6fc6edb3cf1c86719b7b233c0ae",
    );
    let sample_views = read(
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/notice-records/05-notice-views.json"
        )),
        "fc698d032e5b7f8e3ba8d2abf10c62335c5861b5f77b15fcdc3fcc50fb330ee6",
    );
    // Occipital's sample views own their names and data schemas. Compare the
    // shared shapes, not the sample's title fields or template digests.
    let samples = &sample["result"]["notices"];
    assert_eq!(samples["msg_5"]["key"], samples["msg_6"]["key"]);
    let keys = |value: &Value| {
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort_unstable();
        keys
    };
    let metadata = metadata();
    let all_bucket = crate::subc::tool_views::bus_snapshot();
    let writer = Principal::Reserved {
        module_id: "aft".into(),
    };
    for (i, (frame, (sample_id, name))) in frames()
        .iter()
        .zip([
            ("msg_1", "finished"),
            ("msg_5", "still_running"),
            ("msg_4", "watch"),
            ("msg_4", "watch"),
        ])
        .enumerate()
    {
        let body = body(frame, &metadata, metadata.started_at, str::to_owned).unwrap();
        let send_id = format!("aft-notice-{i}");
        let record = pull("broca", "ses_head", &send_id, body).unwrap();
        let wire: Value = serde_json::from_slice(&record.bytes).unwrap();
        assert_eq!(
            keys(&wire),
            ["body", "kind", "runner", "send_id", "session"]
        );
        assert_eq!(wire["kind"], "notice");
        assert_eq!(wire["send_id"], send_id);
        assert_eq!(wire["session"], "ses_head");
        let expected_key = if name == "watch" {
            vec!["data", "view"]
        } else {
            vec!["data", "key", "view"]
        };
        assert_eq!(keys(&wire["body"]), expected_key);
        let write: NoticeWrite = serde_json::from_value(wire).unwrap();
        assert_eq!(parse_notice_view(&write.body.view).unwrap().1, 1);
        let joined = NoticeRecord::from_body(&writer, write.body).unwrap();
        assert_eq!(keys(&json!(joined)), keys(&samples[sample_id]));
        let (sample_writer, sample_name, sample_version) =
            parse_notice_view_key(samples[sample_id]["view"].as_str().unwrap()).unwrap();
        let (actual_writer, actual_name, actual_version) =
            parse_notice_view_key(&joined.view).unwrap();
        assert_eq!(sample_name, name);
        assert_eq!(actual_name, format!("bash_{sample_name}"));
        assert_eq!(
            (actual_writer, actual_version),
            (sample_writer, sample_version)
        );

        // The provider's reply is a one-pin envelope. Occipital's sample is
        // the downstream reply once that entry's templates have been checked.
        let (_, bytes) = all_bucket
            .iter()
            .find(|(pin, _)| pin == &joined.view)
            .unwrap();
        let bucket: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(keys(&bucket), ["views"]);
        assert_eq!(
            keys(&bucket["views"][&joined.view]),
            ["notice", "template", "template_revision"]
        );
        assert_eq!(
            keys(&bucket["views"][&joined.view]["notice"]),
            ["data_schema", "name", "version"]
        );
        let reply: ViewsReply = serde_json::from_value(bucket).unwrap();
        assert_eq!(reply.views.len(), 1);
        let ViewEntry::Notice(view) = &reply.views[&joined.view] else {
            panic!("expected notice view")
        };
        view.check_pin(sample_writer, &joined.view).unwrap();
        let sample_view =
            &sample_views["result"]["views"][samples[sample_id]["view"].as_str().unwrap()];
        assert_eq!(sample_view["provider"], actual_writer);
        assert_eq!(sample_view["templateRevision"], view.template_revision);
        for (slot, document) in [("row", &view.template.row), ("page", &view.template.page)] {
            assert!(sample_view[slot].as_str().unwrap().starts_with("sha256:"));
            assert_eq!(document["profile"], format!("tool_{slot}"));
            assert_eq!(
                document["catalog"]["digest"],
                crate::subc::tool_views::current::CATALOG_DIGEST
            );
        }
    }
}
