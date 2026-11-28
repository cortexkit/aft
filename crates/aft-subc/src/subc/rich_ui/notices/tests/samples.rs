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
        include_bytes!(
            "../../../../../../aft/tests/fixtures/notice-records/phone-reader-notice-records-v2/02-wake-records-with-notices.json"
        ),
        "f6a3c1c6df70a10a79b1abdbc2c4f59e17b8e6fc6edb3cf1c86719b7b233c0ae",
    );
    let sample_views = read(
        include_bytes!("../../../../../../aft/tests/fixtures/notice-records/phone-reader-notice-records-v2/05-notice-views.json"),
        "5c89690069f3fd28b054548f39aa572c9f9e54c8b6a2c1ebddc5ba9a20b342ad",
    );
    // Occipital's sample views own their names and data schemas. Compare the
    // shared shapes, not the sample's title fields or template digests.
    let samples = &sample["result"]["notices"];
    let published: Value = serde_json::from_str(include_str!("../shipped.json")).unwrap();
    let aft_samples = read(
        include_bytes!("../../../../../../aft/tests/fixtures/notice-records/phone-reader-notice-records-aft-v2/04-notice-views.json"),
        "c603f584e840c3c759c24914c0ef1ada10e1a611e86a8326ed9f805878a9cc83",
    );
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
        let aft_sample = &aft_samples["result"]["views"][&joined.view];
        assert_eq!(aft_sample["provider"], actual_writer);
        assert_eq!(
            aft_sample["templateRevision"],
            published["template_revision"]
        );
        let terminal_capable = matches!(name, "finished" | "watch");
        assert_eq!(view.template_revision, if terminal_capable { 3 } else { 2 });
        assert_eq!(view.notice.data_schema, published["data_schema"]);
        for (slot, document, old_digest, digest) in [
            (
                "row",
                &view.template.row,
                "sha256:65f79f660db795ae7a2bd9737beb547e47f5befb759b4d1204e83595bca1aa3b",
                "sha256:06e0e43b9cbea9621d752f141233ebf4f958340340b8f857ca618aa6ade093e1",
            ),
            (
                "page",
                &view.template.page,
                "sha256:7f9f50ce6956a23863a79190243855f30264f9886e5cb12f91e4ec3d35c102de",
                "sha256:a4d40fb50428985e965a91cfdc16cd93f89eccf2fb9e89cea7f1bf8a7b9433f6",
            ),
        ] {
            assert!(sample_view[slot].as_str().unwrap().starts_with("sha256:"));
            assert_eq!(document["profile"], format!("tool_{slot}"));
            assert_eq!(
                document["catalog"]["digest"],
                crate::subc::tool_views::current::CATALOG_DIGEST
            );
            let hash = |value: &Value| {
                occipital_registry::Template::parse(&serde_json::to_vec(value).unwrap())
                    .unwrap()
                    .digest()
                    .unwrap()
            };
            let mut prior = published["template"][slot].clone();
            assert_eq!(hash(&prior), old_digest);
            prior["catalog"]["digest"] = document["catalog"]["digest"].clone();
            if terminal_capable {
                assert_eq!(hash(&prior), digest);
                prior["source"] = document["source"].clone();
            }
            let digest = match (name, slot) {
                ("finished" | "watch", "row") => {
                    "sha256:53f6286e704b9de70510c7dcce03b7fbe89991cdd2bc27c32d510f0a688fe59e"
                }
                ("finished", "page") => {
                    "sha256:7a920615fa74f40654b4d34ebb59bba9c09e26b8a05826bc43909eb1f8c9a7e1"
                }
                ("watch", "page") => {
                    "sha256:6d77314d5b03d3d40743b632bccf6f85c41ec693bd0bad570f7e953dcf781fc0"
                }
                _ => digest,
            };
            assert_eq!(hash(document), digest);
            assert_eq!(&prior, document);
        }
    }
}
